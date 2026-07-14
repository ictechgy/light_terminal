//! Durable, Linux-only managed root-process launch substrate.
//!
//! This module is intentionally disconnected from ordinary sessions and the
//! public protocol.  A future feature may call `launch_managed_process`; until
//! then only the hidden gate dispatcher is wired into the binary.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 1;
const SLOT_COUNT: usize = 1_024;
const MAX_RECORD_BYTES: usize = 8 * 1_024;
const TOMBSTONE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const INTERNAL_GATE_ARG: &str = "__lterm-internal-managed-launch-gate-v1";
#[cfg(debug_assertions)]
const INTERNAL_TEST_LAUNCH_ARG: &str = "__lterm-internal-managed-launch-test-v1";
#[cfg(debug_assertions)]
const INTERNAL_TEST_RECONCILE_ARG: &str = "__lterm-internal-managed-reconcile-test-v1";
#[cfg(target_os = "linux")]
const GATE_CONTROL_FD: RawFd = 3;
#[cfg(target_os = "linux")]
const GATE_GUARD_FD: RawFd = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "evidence", content = "value")]
pub(crate) enum Evidence<T> {
    Present(T),
    Absent,
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcessIdentity {
    pub boot_uuid: Uuid,
    pub pid_namespace_inode: u64,
    pub pid: u32,
    pub start_ticks: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
enum SlotState {
    Vacant,
    IntentDurable {
        nonce: Uuid,
        created_unix_secs: u64,
    },
    IdentityDurable {
        nonce: Uuid,
        identity: ProcessIdentity,
        release_may_have_occurred: bool,
    },
    CleanupPending {
        nonce: Uuid,
        identity: ProcessIdentity,
        release_may_have_occurred: bool,
    },
    ResolvedTombstone {
        nonce: Uuid,
        identity: Option<ProcessIdentity>,
        resolved_unix_secs: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SlotRecord {
    schema_version: u32,
    slot: u16,
    generation: u64,
    #[serde(flatten)]
    state: SlotState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GateRegistration {
    schema_version: u32,
    slot: u16,
    generation: u64,
    nonce: Uuid,
    identity: ProcessIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RegistrationRecord {
    schema_version: u32,
    slot: u16,
    generation: u64,
    registration: Option<GateRegistration>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GateHello {
    protocol: String,
    registration: GateRegistration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GateCommit {
    protocol: String,
    slot: u16,
    generation: u64,
    nonce: Uuid,
    identity: ProcessIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GateExecFailure {
    protocol: String,
    errno: Option<i32>,
    message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReconcileOutcome {
    Absent,
    Live,
    UnknownOrphanRisk(String),
    ResolvedTombstone,
}

#[derive(Clone, Debug)]
pub(crate) struct ManagedLaunchRequest {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub environment: Vec<(OsString, OsString)>,
}

#[derive(Debug)]
pub(crate) struct ManagedProcess {
    pub slot: u16,
    pub generation: u64,
    pub identity: ProcessIdentity,
    child: Option<std::process::Child>,
    registry: Registry,
}

impl ManagedProcess {
    #[cfg(target_os = "linux")]
    pub(crate) fn wait(mut self) -> Result<std::process::ExitStatus> {
        let status = self
            .child
            .take()
            .context("managed root-process handle was already consumed")?
            .wait()
            .context("wait for managed root process")?;
        ensure!(
            self.registry.cleanup(self.slot, self.generation)?
                == ReconcileOutcome::ResolvedTombstone,
            "managed root process exited without a durable tombstone"
        );
        Ok(status)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn terminate_and_wait(mut self) -> Result<std::process::ExitStatus> {
        let first = self.registry.cleanup(self.slot, self.generation)?;
        ensure!(
            matches!(
                first,
                ReconcileOutcome::ResolvedTombstone | ReconcileOutcome::UnknownOrphanRisk(_)
            ),
            "managed cleanup did not start conservatively: {first:?}"
        );
        let status = self
            .child
            .take()
            .context("managed root-process handle was already consumed")?
            .wait()
            .context("reap managed root process after cleanup")?;
        ensure!(
            self.registry.cleanup(self.slot, self.generation)?
                == ReconcileOutcome::ResolvedTombstone,
            "managed cleanup did not reach a durable tombstone after reap"
        );
        Ok(status)
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(mut child) = self.child.take() {
            let registry = self.registry.clone();
            let slot = self.slot;
            let generation = self.generation;
            let _ = std::thread::Builder::new()
                .name("lterm-managed-root-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                    let _ = registry.cleanup(slot, generation);
                });
        }
    }
}

#[derive(Clone, Debug)]
struct Registry {
    root: PathBuf,
    slots: PathBuf,
    guards: PathBuf,
    registrations: PathBuf,
    slot_count: usize,
}

#[derive(Debug)]
struct OfdLock {
    file: File,
}

#[derive(Debug)]
struct LaunchIntent {
    record: SlotRecord,
    guard: OfdLock,
}

#[derive(Debug)]
enum SlotRead {
    Valid(SlotRecord),
    Unknown(String),
}

impl Registry {
    #[cfg(target_os = "linux")]
    fn open_default() -> Result<Self> {
        Self::open_at(crate::paths::process_registry_dir()?, SLOT_COUNT)
    }

    fn open_at(root: PathBuf, slot_count: usize) -> Result<Self> {
        ensure!(slot_count > 0 && slot_count <= u16::MAX as usize);
        let registry = Self {
            slots: root.join("slots"),
            guards: root.join("guards"),
            registrations: root.join("registrations"),
            root,
            slot_count,
        };
        registry.ensure_genesis()?;
        registry.validate_layout()?;
        Ok(registry)
    }

    fn ensure_genesis(&self) -> Result<()> {
        match fs::symlink_metadata(&self.root) {
            Ok(_) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("lstat {}", self.root.display())),
        }

        let parent = self.root.parent().context("registry root has no parent")?;
        ensure_exact_private_dir(parent)?;
        let leaf = self
            .root
            .file_name()
            .context("registry root has no file name")?
            .to_string_lossy();
        let temp = parent.join(format!(".{leaf}.genesis-{}", Uuid::new_v4()));
        let cleanup = TempTree::new(temp.clone());

        create_exact_dir(&temp)?;
        let slots = temp.join("slots");
        let guards = temp.join("guards");
        let registrations = temp.join("registrations");
        create_exact_dir(&slots)?;
        create_exact_dir(&guards)?;
        create_exact_dir(&registrations)?;
        create_exact_file(&temp.join("registry.lock"), b"")?;

        for slot in 0..self.slot_count {
            let slot = u16::try_from(slot).context("slot index overflow")?;
            let record = SlotRecord {
                schema_version: SCHEMA_VERSION,
                slot,
                generation: 0,
                state: SlotState::Vacant,
            };
            create_exact_json_file(&slots.join(slot_name(slot)), &record)?;
            create_exact_file(&guards.join(guard_name(slot)), b"")?;
            let registration = RegistrationRecord {
                schema_version: SCHEMA_VERSION,
                slot,
                generation: 0,
                registration: None,
            };
            create_exact_json_file(&registrations.join(registration_name(slot)), &registration)?;
        }

        sync_dir(&slots)?;
        sync_dir(&guards)?;
        sync_dir(&registrations)?;
        sync_dir(&temp)?;
        match rename_noreplace(&temp, &self.root) {
            Ok(()) => cleanup.disarm(),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                drop(cleanup);
            }
            Err(err) => return Err(err).context("install registry genesis with no-replace rename"),
        }
        sync_dir(parent)?;
        Ok(())
    }

    fn validate_layout(&self) -> Result<()> {
        validate_exact_dir(&self.root)?;
        validate_exact_dir(&self.slots)?;
        validate_exact_dir(&self.guards)?;
        validate_exact_dir(&self.registrations)?;
        validate_exact_file(&self.root.join("registry.lock"))?;
        for slot in 0..self.slot_count {
            let slot = u16::try_from(slot).context("slot index overflow")?;
            validate_exact_file(&self.slots.join(slot_name(slot)))?;
            validate_exact_file(&self.guards.join(guard_name(slot)))?;
            validate_exact_file(&self.registrations.join(registration_name(slot)))?;
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn allocate_intent(&self, now_unix_secs: u64) -> Result<LaunchIntent> {
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed launch registry is busy")?;
        let mut selected = None;
        let mut unknown = 0usize;

        for slot in 0..self.slot_count {
            let slot = u16::try_from(slot).context("slot index overflow")?;
            match self.read_slot(slot) {
                SlotRead::Valid(record) => match &record.state {
                    SlotState::Vacant => {
                        selected.get_or_insert(record);
                    }
                    SlotState::ResolvedTombstone {
                        resolved_unix_secs, ..
                    } => {
                        if now_unix_secs
                            .checked_sub(*resolved_unix_secs)
                            .is_some_and(|age| age >= TOMBSTONE_RETENTION.as_secs())
                        {
                            let vacant = SlotRecord {
                                state: SlotState::Vacant,
                                ..record.clone()
                            };
                            self.replace_slot(&record, &vacant)?;
                            selected.get_or_insert(vacant);
                        } else {
                            unknown += 1;
                        }
                    }
                    _ => unknown += 1,
                },
                SlotRead::Unknown(_) => unknown += 1,
            }
        }

        let vacant = selected.with_context(|| {
            format!(
                "unknown_orphan_risk: managed registry capacity exhausted ({unknown}/{} unresolved)",
                self.slot_count
            )
        })?;
        let generation = vacant
            .generation
            .checked_add(1)
            .context("slot generation exhausted; generation must never wrap")?;
        let nonce = Uuid::new_v4();

        self.replace_registration(
            vacant.slot,
            &RegistrationRecord {
                schema_version: SCHEMA_VERSION,
                slot: vacant.slot,
                generation,
                registration: None,
            },
        )?;
        let intent = SlotRecord {
            schema_version: SCHEMA_VERSION,
            slot: vacant.slot,
            generation,
            state: SlotState::IntentDurable {
                nonce,
                created_unix_secs: now_unix_secs,
            },
        };
        self.replace_slot(&vacant, &intent)?;

        // This ordering is intentional: the intent is durable before a child
        // can inherit the fixed guard's open file description.
        let guard = OfdLock::try_acquire(&self.guards.join(guard_name(vacant.slot)))
            .context("unknown_orphan_risk: intent durable but slot guard is busy")?;
        Ok(LaunchIntent {
            record: intent,
            guard,
        })
    }

    fn read_slot(&self, slot: u16) -> SlotRead {
        let path = self.slots.join(slot_name(slot));
        match read_bounded_json::<SlotRecord>(&path).and_then(|record| {
            validate_slot_record(&record, slot)?;
            Ok(record)
        }) {
            Ok(record) => SlotRead::Valid(record),
            Err(err) => SlotRead::Unknown(format!("{}: {err:#}", path.display())),
        }
    }

    fn read_valid_slot(&self, slot: u16) -> Result<SlotRecord> {
        match self.read_slot(slot) {
            SlotRead::Valid(record) => Ok(record),
            SlotRead::Unknown(reason) => bail!("unknown_orphan_risk: {reason}"),
        }
    }

    fn replace_slot(&self, expected: &SlotRecord, next: &SlotRecord) -> Result<()> {
        validate_transition(expected, next)?;
        let current = self.read_valid_slot(expected.slot)?;
        ensure!(
            current == *expected,
            "generation/state conflict replacing slot {}",
            expected.slot
        );
        atomic_replace_json(&self.slots, &slot_name(expected.slot), next)?;
        let readback = self.read_valid_slot(expected.slot)?;
        ensure!(readback == *next, "slot durable readback mismatch");
        Ok(())
    }

    fn read_registration(&self, slot: u16) -> Result<RegistrationRecord> {
        let path = self.registrations.join(registration_name(slot));
        let record = read_bounded_json::<RegistrationRecord>(&path)?;
        ensure!(record.schema_version == SCHEMA_VERSION);
        ensure!(record.slot == slot);
        Ok(record)
    }

    fn replace_registration(&self, slot: u16, next: &RegistrationRecord) -> Result<()> {
        ensure!(next.schema_version == SCHEMA_VERSION && next.slot == slot);
        if let Some(registration) = &next.registration {
            ensure!(registration.schema_version == SCHEMA_VERSION);
            ensure!(registration.slot == slot);
            ensure!(registration.generation == next.generation);
        }
        atomic_replace_json(&self.registrations, &registration_name(slot), next)?;
        ensure!(self.read_registration(slot)? == *next);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn record_identity(
        &self,
        intent: &SlotRecord,
        registration: &GateRegistration,
    ) -> Result<SlotRecord> {
        let SlotState::IntentDurable { nonce, .. } = intent.state else {
            bail!("slot is not IntentDurable");
        };
        ensure!(registration.slot == intent.slot);
        ensure!(registration.generation == intent.generation);
        ensure!(registration.nonce == nonce);
        let durable_registration = self.read_registration(intent.slot)?;
        ensure!(
            durable_registration.registration.as_ref() == Some(registration),
            "gate registration durable readback mismatch"
        );
        let identity = SlotRecord {
            schema_version: SCHEMA_VERSION,
            slot: intent.slot,
            generation: intent.generation,
            state: SlotState::IdentityDurable {
                nonce,
                identity: registration.identity.clone(),
                // Conservative before COMMIT: a crash after the durable write
                // cannot prove whether the packet was delivered.
                release_may_have_occurred: true,
            },
        };
        self.replace_slot(intent, &identity)?;
        Ok(identity)
    }

    #[cfg(target_os = "linux")]
    fn reconcile_intent(&self, record: &SlotRecord) -> ReconcileOutcome {
        let SlotState::IntentDurable { nonce, .. } = record.state else {
            return ReconcileOutcome::UnknownOrphanRisk("not an intent record".into());
        };
        match OfdLock::try_acquire(&self.guards.join(guard_name(record.slot))) {
            Ok(_guard) => ReconcileOutcome::Absent,
            Err(_) => match self.read_registration(record.slot) {
                Ok(sidecar)
                    if sidecar.generation == record.generation
                        && sidecar.registration.as_ref().is_some_and(|registration| {
                            registration.nonce == nonce
                                && registration.slot == record.slot
                                && registration.generation == record.generation
                        }) =>
                {
                    let registration = sidecar.registration.expect("checked some");
                    match verify_exact_process(&registration.identity) {
                        Evidence::Present(_) => ReconcileOutcome::Live,
                        Evidence::Absent => ReconcileOutcome::UnknownOrphanRisk(
                            "busy intent guard has an absent registered identity".into(),
                        ),
                        Evidence::Unavailable(reason) => {
                            ReconcileOutcome::UnknownOrphanRisk(reason)
                        }
                    }
                }
                Ok(_) => ReconcileOutcome::UnknownOrphanRisk(
                    "busy intent guard has missing or stale registration sidecar".into(),
                ),
                Err(err) => ReconcileOutcome::UnknownOrphanRisk(format!(
                    "busy intent guard registration unavailable: {err:#}"
                )),
            },
        }
    }

    #[cfg(target_os = "linux")]
    fn cleanup(&self, slot: u16, generation: u64) -> Result<ReconcileOutcome> {
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed launch registry is busy")?;
        let current = self.read_valid_slot(slot)?;
        ensure!(current.generation == generation, "generation conflict");
        let (nonce, identity, release_may_have_occurred) = match &current.state {
            SlotState::IdentityDurable {
                nonce,
                identity,
                release_may_have_occurred,
            }
            | SlotState::CleanupPending {
                nonce,
                identity,
                release_may_have_occurred,
            } => (*nonce, identity.clone(), *release_may_have_occurred),
            SlotState::ResolvedTombstone { .. } => {
                return Ok(ReconcileOutcome::ResolvedTombstone);
            }
            SlotState::IntentDurable { nonce, .. } => {
                return match self.reconcile_intent(&current) {
                    ReconcileOutcome::Absent => self.persist_tombstone(&current, *nonce, None),
                    outcome => Ok(outcome),
                };
            }
            SlotState::Vacant => return Ok(ReconcileOutcome::Absent),
        };

        let pending = SlotRecord {
            state: SlotState::CleanupPending {
                nonce,
                identity: identity.clone(),
                release_may_have_occurred,
            },
            ..current.clone()
        };
        if current != pending {
            self.replace_slot(&current, &pending)?;
        }
        managed_test_failpoint("after_cleanup_pending");

        match open_verified_pidfd(&identity) {
            Evidence::Present(pidfd) => {
                pidfd.send_signal(libc::SIGKILL)?;
                managed_test_failpoint("after_cleanup_signal");
                pidfd.wait(Duration::from_secs(5))?;
                match verify_exact_process(&identity) {
                    Evidence::Absent => self.persist_tombstone(&pending, nonce, Some(identity)),
                    Evidence::Present(_) => Ok(ReconcileOutcome::UnknownOrphanRisk(
                        "matching process remained live after pidfd signal".into(),
                    )),
                    Evidence::Unavailable(reason) => {
                        Ok(ReconcileOutcome::UnknownOrphanRisk(reason))
                    }
                }
            }
            Evidence::Absent => self.persist_tombstone(&pending, nonce, Some(identity)),
            Evidence::Unavailable(reason) => Ok(ReconcileOutcome::UnknownOrphanRisk(reason)),
        }
    }

    fn persist_tombstone(
        &self,
        current: &SlotRecord,
        nonce: Uuid,
        identity: Option<ProcessIdentity>,
    ) -> Result<ReconcileOutcome> {
        let tombstone = SlotRecord {
            state: SlotState::ResolvedTombstone {
                nonce,
                identity,
                resolved_unix_secs: now_unix_secs()?,
            },
            ..current.clone()
        };
        #[cfg(target_os = "linux")]
        managed_test_failpoint("before_tombstone");
        self.replace_slot(current, &tombstone)?;
        #[cfg(target_os = "linux")]
        managed_test_failpoint("after_tombstone");
        Ok(ReconcileOutcome::ResolvedTombstone)
    }

    #[cfg(target_os = "linux")]
    fn reconcile_all(&self) -> Vec<(u16, ReconcileOutcome)> {
        (0..self.slot_count)
            .map(|slot| u16::try_from(slot).expect("validated registry slot count"))
            .filter_map(|slot| match self.read_slot(slot) {
                SlotRead::Valid(SlotRecord {
                    state: SlotState::Vacant | SlotState::ResolvedTombstone { .. },
                    ..
                }) => None,
                SlotRead::Valid(record) => Some((
                    slot,
                    self.cleanup(slot, record.generation).unwrap_or_else(|err| {
                        ReconcileOutcome::UnknownOrphanRisk(format!(
                            "slot {slot} reconciliation failed: {err:#}"
                        ))
                    }),
                )),
                SlotRead::Unknown(reason) => {
                    Some((slot, ReconcileOutcome::UnknownOrphanRisk(reason)))
                }
            })
            .collect()
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn reconcile_managed_processes() -> Result<Vec<(u16, ReconcileOutcome)>> {
    Ok(Registry::open_default()?.reconcile_all())
}

fn validate_slot_record(record: &SlotRecord, expected_slot: u16) -> Result<()> {
    ensure!(record.schema_version == SCHEMA_VERSION);
    ensure!(record.slot == expected_slot);
    match &record.state {
        SlotState::Vacant => {}
        SlotState::IntentDurable { nonce, .. }
        | SlotState::IdentityDurable { nonce, .. }
        | SlotState::CleanupPending { nonce, .. }
        | SlotState::ResolvedTombstone { nonce, .. } => ensure!(!nonce.is_nil()),
    }
    Ok(())
}

fn validate_transition(current: &SlotRecord, next: &SlotRecord) -> Result<()> {
    ensure!(current.schema_version == SCHEMA_VERSION);
    ensure!(next.schema_version == SCHEMA_VERSION);
    ensure!(current.slot == next.slot);
    let legal = match (&current.state, &next.state) {
        (SlotState::Vacant, SlotState::IntentDurable { .. }) => current
            .generation
            .checked_add(1)
            .is_some_and(|generation| generation == next.generation),
        (
            SlotState::IntentDurable { nonce: a, .. },
            SlotState::IdentityDurable { nonce: b, .. },
        )
        | (
            SlotState::IdentityDurable { nonce: a, .. },
            SlotState::CleanupPending { nonce: b, .. },
        )
        | (
            SlotState::CleanupPending { nonce: a, .. },
            SlotState::ResolvedTombstone { nonce: b, .. },
        ) => current.generation == next.generation && a == b,
        (
            SlotState::IntentDurable { nonce: a, .. },
            SlotState::ResolvedTombstone { nonce: b, .. },
        ) => current.generation == next.generation && a == b,
        (SlotState::ResolvedTombstone { .. }, SlotState::Vacant) => {
            current.generation == next.generation
        }
        // Idempotent durable readback/rewrite is permitted only for the same
        // full record, never as a state shortcut.
        _ => current == next,
    };
    ensure!(legal, "illegal managed registry state transition");
    Ok(())
}

fn slot_name(slot: u16) -> String {
    format!("slot-{slot:04}.json")
}

fn guard_name(slot: u16) -> String {
    format!("guard-{slot:04}")
}

fn registration_name(slot: u16) -> String {
    format!("registration-{slot:04}.json")
}

fn now_unix_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs())
}

fn create_exact_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create directory {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    validate_exact_dir(path)
}

fn ensure_exact_private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        let parent = path.parent().context("private directory has no parent")?;
        if !parent.exists() {
            ensure_exact_private_dir(parent)?;
        }
        create_exact_dir(path)?;
        sync_dir(parent)?;
    }
    validate_exact_dir(path)
}

fn create_exact_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    ensure!(bytes.len() <= MAX_RECORD_BYTES);
    create_exact_file(path, &bytes)
}

fn create_exact_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    validate_file_handle(&file, path)
}

fn atomic_replace_json<T: Serialize>(directory: &Path, name: &str, value: &T) -> Result<()> {
    validate_exact_dir(directory)?;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() <= MAX_RECORD_BYTES,
        "record exceeds 8 KiB bound"
    );
    let temp_name = format!(".{name}.{}.tmp", Uuid::new_v4());
    let temp = directory.join(&temp_name);
    create_exact_file(&temp, &bytes)?;
    validate_exact_file(&temp)?;
    fs::rename(&temp, directory.join(name))?;
    sync_dir(directory)?;
    Ok(())
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let file = open_exact_file(path, false)?;
    let length = file.metadata()?.len();
    ensure!(
        length <= MAX_RECORD_BYTES as u64,
        "record exceeds 8 KiB bound"
    );
    let mut bytes = Vec::with_capacity(length as usize);
    file.take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_RECORD_BYTES,
        "record exceeds 8 KiB bound"
    );
    serde_json::from_slice(&bytes).context("parse durable JSON record")
}

fn open_exact_file(path: &Path, write: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.write(write);
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    validate_file_handle(&file, path)?;
    Ok(file)
}

fn validate_exact_file(path: &Path) -> Result<()> {
    let _ = open_exact_file(path, false)?;
    Ok(())
}

fn validate_file_handle(file: &File, path: &Path) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "{} is not a regular file",
        path.display()
    );
    ensure!(metadata.uid() == crate::paths::current_euid());
    ensure!(
        metadata.nlink() == 1,
        "{} must have nlink=1",
        path.display()
    );
    ensure!(metadata.permissions().mode() & 0o777 == 0o600);
    Ok(())
}

fn validate_exact_dir(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(!metadata.file_type().is_symlink());
    ensure!(metadata.is_dir());
    ensure!(metadata.uid() == crate::paths::current_euid());
    ensure!(metadata.permissions().mode() & 0o777 == 0o700);
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    file.sync_all()?;
    Ok(())
}

struct TempTree {
    path: PathBuf,
    armed: std::cell::Cell<bool>,
}

impl TempTree {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            armed: std::cell::Cell::new(true),
        }
    }

    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        if self.armed.get() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(target_os = "linux")]
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    // Non-Linux builds never launch managed processes. This fallback exists so
    // platform-neutral durability/state tests can exercise the file model.
    fs::rename(from, to)
}

#[cfg(target_os = "linux")]
impl OfdLock {
    fn try_acquire(path: &Path) -> Result<Self> {
        let file = open_exact_file(path, true)?;
        let mut lock = libc::flock {
            l_type: libc::F_WRLCK as i16,
            l_whence: libc::SEEK_SET as i16,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &mut lock) };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("acquire exclusive OFD lock");
        }
        Ok(Self { file })
    }
}

#[cfg(target_os = "linux")]
fn read_boot_uuid() -> Evidence<Uuid> {
    match fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        Ok(text) => match Uuid::parse_str(text.trim()) {
            Ok(uuid) => Evidence::Present(uuid),
            Err(err) => Evidence::Unavailable(format!("invalid boot UUID: {err}")),
        },
        Err(err) => Evidence::Unavailable(format!("boot UUID unavailable: {err}")),
    }
}

#[cfg(target_os = "linux")]
fn read_pid_namespace_inode(pid: u32) -> Evidence<u64> {
    match fs::metadata(format!("/proc/{pid}/ns/pid")) {
        Ok(metadata) => Evidence::Present(metadata.ino()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Evidence::Absent,
        Err(err) => Evidence::Unavailable(format!("PID namespace unavailable: {err}")),
    }
}

fn parse_proc_stat_start_ticks(stat: &str) -> Result<u64> {
    let close = stat
        .rfind(')')
        .context("proc stat has no closing command delimiter")?;
    let rest = stat
        .get(close + 1..)
        .context("proc stat delimiter is invalid")?;
    let value = rest
        .split_whitespace()
        .nth(19)
        .context("proc stat is missing field 22")?;
    value.parse().context("proc stat field 22 is invalid")
}

#[cfg(target_os = "linux")]
fn read_start_ticks(pid: u32) -> Evidence<u64> {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => match parse_proc_stat_start_ticks(&stat) {
            Ok(ticks) => Evidence::Present(ticks),
            Err(err) => Evidence::Unavailable(format!("start ticks unavailable: {err:#}")),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Evidence::Absent,
        Err(err) => Evidence::Unavailable(format!("start ticks unavailable: {err}")),
    }
}

#[cfg(target_os = "linux")]
fn observe_identity(pid: u32) -> Evidence<ProcessIdentity> {
    let boot_uuid = match read_boot_uuid() {
        Evidence::Present(value) => value,
        Evidence::Absent => return Evidence::Unavailable("boot UUID unexpectedly absent".into()),
        Evidence::Unavailable(reason) => return Evidence::Unavailable(reason),
    };
    let pid_namespace_inode = match read_pid_namespace_inode(pid) {
        Evidence::Present(value) => value,
        Evidence::Absent => return Evidence::Absent,
        Evidence::Unavailable(reason) => return Evidence::Unavailable(reason),
    };
    let start_ticks = match read_start_ticks(pid) {
        Evidence::Present(value) => value,
        Evidence::Absent => return Evidence::Absent,
        Evidence::Unavailable(reason) => return Evidence::Unavailable(reason),
    };
    Evidence::Present(ProcessIdentity {
        boot_uuid,
        pid_namespace_inode,
        pid,
        start_ticks,
    })
}

#[cfg(target_os = "linux")]
fn verify_exact_process(expected: &ProcessIdentity) -> Evidence<ProcessIdentity> {
    match read_boot_uuid() {
        Evidence::Present(boot) if boot != expected.boot_uuid => return Evidence::Absent,
        Evidence::Present(_) => {}
        Evidence::Absent => return Evidence::Unavailable("boot UUID unexpectedly absent".into()),
        Evidence::Unavailable(reason) => return Evidence::Unavailable(reason),
    }
    match open_verified_pidfd(expected) {
        Evidence::Present(_pidfd) => Evidence::Present(expected.clone()),
        Evidence::Absent => Evidence::Absent,
        Evidence::Unavailable(reason) => Evidence::Unavailable(reason),
    }
}

#[cfg(target_os = "linux")]
struct PidFd(File);

#[cfg(target_os = "linux")]
impl PidFd {
    fn open(pid: u32) -> Evidence<Self> {
        let result = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if result >= 0 {
            use std::os::fd::FromRawFd;
            return Evidence::Present(Self(unsafe { File::from_raw_fd(result as RawFd) }));
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            Evidence::Absent
        } else {
            Evidence::Unavailable(format!("pidfd_open unavailable: {err}"))
        }
    }

    fn send_signal(&self, signal: i32) -> Result<()> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(err).context("pidfd_send_signal")
            }
        }
    }

    fn wait(&self, timeout: Duration) -> Result<()> {
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let mut pollfd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
        if result < 0 {
            Err(std::io::Error::last_os_error()).context("poll pidfd")
        } else if result == 0 {
            bail!("timed out waiting for pidfd readiness")
        } else {
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn open_verified_pidfd(expected: &ProcessIdentity) -> Evidence<PidFd> {
    let pidfd = match PidFd::open(expected.pid) {
        Evidence::Present(pidfd) => pidfd,
        Evidence::Absent => return Evidence::Absent,
        Evidence::Unavailable(reason) => return Evidence::Unavailable(reason),
    };
    // The reread happens after pidfd_open. A mismatch proves that the recorded
    // incarnation is absent, but the current PID must never be signalled.
    match observe_identity(expected.pid) {
        Evidence::Present(actual) if actual == *expected => Evidence::Present(pidfd),
        Evidence::Present(_) | Evidence::Absent => Evidence::Absent,
        Evidence::Unavailable(reason) => Evidence::Unavailable(reason),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn launch_managed_process(request: ManagedLaunchRequest) -> Result<ManagedProcess> {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let registry = Registry::open_default()?;
    managed_test_failpoint("parent_before_intent");
    let intent = registry.allocate_intent(now_unix_secs()?)?;
    managed_test_failpoint("parent_after_intent");
    let SlotState::IntentDurable { nonce, .. } = &intent.record.state else {
        unreachable!("allocator returns intent")
    };
    let nonce = *nonce;
    let target = match open_pinned_executable(&request.executable) {
        Ok(target) => target,
        Err(err) => return Err(resolve_unspawned_intent(&registry, intent, err)),
    };
    let (parent_control, child_control) = match seqpacket_pair() {
        Ok(pair) => pair,
        Err(err) => return Err(resolve_unspawned_intent(&registry, intent, err)),
    };
    let guard_fd = intent.guard.file.as_raw_fd();
    let control_fd = child_control.as_raw_fd();

    let mut command = Command::new("/proc/self/exe");
    command
        .arg(INTERNAL_GATE_ARG)
        .arg(&registry.root)
        .arg(intent.record.slot.to_string())
        .arg(intent.record.generation.to_string())
        .arg(nonce.to_string())
        .arg(request.executable.as_os_str())
        .args(&request.arguments)
        .env_clear()
        .envs(request.environment);
    if let Some(current_dir) = request.current_dir {
        command.current_dir(current_dir);
    }
    unsafe {
        command.pre_exec(move || remap_gate_fds(control_fd, guard_fd));
    }
    let mut child = match command.spawn().context("spawn trusted managed launch gate") {
        Ok(child) => child,
        Err(err) => return Err(resolve_unspawned_intent(&registry, intent, err)),
    };
    managed_test_failpoint("parent_after_spawn");
    drop(child_control);
    let launch_result = (|| -> Result<ProcessIdentity> {
        set_socket_timeout(parent_control.as_raw_fd(), Duration::from_secs(10))?;
        let hello: GateHello = recv_packet(parent_control.as_raw_fd())?;
        ensure!(hello.protocol == "lterm-managed-hello-v1");
        ensure!(hello.registration.slot == intent.record.slot);
        ensure!(hello.registration.generation == intent.record.generation);
        ensure!(hello.registration.nonce == nonce);
        ensure!(hello.registration.identity.pid == child.id());
        managed_test_failpoint("parent_after_hello");
        let verified = match open_verified_pidfd(&hello.registration.identity) {
            Evidence::Present(pidfd) => pidfd,
            Evidence::Absent => bail!("managed gate exited before identity promotion"),
            Evidence::Unavailable(reason) => bail!("managed gate identity unavailable: {reason}"),
        };
        drop(verified);
        let identity_record = registry.record_identity(&intent.record, &hello.registration)?;
        managed_test_failpoint("parent_after_identity");
        let SlotState::IdentityDurable { identity, .. } = &identity_record.state else {
            unreachable!()
        };
        let commit = GateCommit {
            protocol: "lterm-managed-commit-v1".into(),
            slot: intent.record.slot,
            generation: intent.record.generation,
            nonce,
            identity: identity.clone(),
        };
        managed_test_failpoint("parent_before_commit");
        send_commit_with_fd(parent_control.as_raw_fd(), &commit, target.as_raw_fd())?;
        managed_test_failpoint("parent_after_commit");
        wait_for_gate_exec(parent_control.as_raw_fd())?;
        Ok(identity.clone())
    })();

    match launch_result {
        Ok(identity) => Ok(ManagedProcess {
            slot: intent.record.slot,
            generation: intent.record.generation,
            identity,
            child: Some(child),
            registry,
        }),
        Err(err) => {
            drop(parent_control);
            settle_failed_launch(
                &registry,
                intent.record.slot,
                intent.record.generation,
                &mut child,
            );
            Err(err)
        }
    }
}

#[cfg(target_os = "linux")]
fn resolve_unspawned_intent(
    registry: &Registry,
    intent: LaunchIntent,
    error: anyhow::Error,
) -> anyhow::Error {
    let slot = intent.record.slot;
    let generation = intent.record.generation;
    drop(intent);
    match registry.cleanup(slot, generation) {
        Ok(ReconcileOutcome::ResolvedTombstone) => error,
        Ok(outcome) => error.context(format!(
            "pre-spawn intent did not reach a tombstone: {outcome:?}"
        )),
        Err(cleanup_error) => error.context(format!(
            "pre-spawn intent cleanup failed: {cleanup_error:#}"
        )),
    }
}

#[cfg(target_os = "linux")]
fn wait_for_gate_exec(control_fd: RawFd) -> Result<()> {
    let mut bytes = [0u8; MAX_RECORD_BYTES + 1];
    let received = unsafe {
        libc::recv(
            control_fd,
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            libc::MSG_TRUNC,
        )
    };
    if received == 0 {
        return Ok(());
    }
    if received < 0 {
        return Err(std::io::Error::last_os_error()).context("wait for managed target exec");
    }
    ensure!(
        received as usize <= MAX_RECORD_BYTES,
        "oversized gate exec status"
    );
    let failure: GateExecFailure = serde_json::from_slice(&bytes[..received as usize])
        .context("malformed gate exec failure")?;
    ensure!(failure.protocol == "lterm-managed-exec-failure-v1");
    bail!(
        "managed target exec failed{}: {}",
        failure
            .errno
            .map(|errno| format!(" (errno {errno})"))
            .unwrap_or_default(),
        failure.message
    )
}

#[cfg(target_os = "linux")]
fn settle_failed_launch(
    registry: &Registry,
    slot: u16,
    generation: u64,
    child: &mut std::process::Child,
) {
    if !reap_child_until(child, Duration::from_secs(2)).unwrap_or(false) {
        let _ = registry.cleanup(slot, generation);
        let _ = reap_child_until(child, Duration::from_secs(5));
    }
    let _ = registry.cleanup(slot, generation);
}

#[cfg(target_os = "linux")]
fn reap_child_until(child: &mut std::process::Child, timeout: Duration) -> Result<bool> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
<<<<<<< HEAD
fn managed_test_failpoint(name: &str) {
    #[cfg(debug_assertions)]
    if std::env::var("LTERM_INTERNAL_MANAGED_FAILPOINT").as_deref() == Ok(name) {
=======
fn managed_test_failpoint(_name: &str) {
    #[cfg(debug_assertions)]
    if std::env::var("LTERM_INTERNAL_MANAGED_FAILPOINT").as_deref() == Ok(_name) {
>>>>>>> 6b51e05
        unsafe { libc::_exit(86) };
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn launch_managed_process(_request: ManagedLaunchRequest) -> Result<ManagedProcess> {
    bail!("durable managed-process launch is supported only on Linux")
}

pub(crate) fn dispatch_internal_gate() -> Result<bool> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new(INTERNAL_GATE_ARG)) {
        return Ok(false);
    }
    #[cfg(target_os = "linux")]
    {
        run_gate(arguments.collect())?;
        Ok(true)
    }
    #[cfg(not(target_os = "linux"))]
    {
        bail!("internal managed launch gate is supported only on Linux")
    }
}

#[cfg(debug_assertions)]
pub(crate) fn dispatch_internal_test_driver() -> Result<bool> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let action = arguments.next();
    if action.as_deref() != Some(std::ffi::OsStr::new(INTERNAL_TEST_LAUNCH_ARG))
        && action.as_deref() != Some(std::ffi::OsStr::new(INTERNAL_TEST_RECONCILE_ARG))
    {
        return Ok(false);
    }
    ensure!(
        std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref() == Some(std::ffi::OsStr::new("1")),
        "internal managed-launch test driver requires LTERM_INTERNAL_TEST_MODE=1"
    );
    #[cfg(not(target_os = "linux"))]
    bail!("internal managed-launch test driver is supported only on Linux");
    #[cfg(target_os = "linux")]
    if action.as_deref() == Some(std::ffi::OsStr::new(INTERNAL_TEST_RECONCILE_ARG)) {
        for (slot, outcome) in reconcile_managed_processes()? {
            println!("managed-reconcile slot={slot} outcome={outcome:?}");
        }
        return Ok(true);
    }
    #[cfg(target_os = "linux")]
    {
        let executable = arguments
            .next()
            .map(PathBuf::from)
            .context("internal managed-launch test driver requires an executable")?;
        let process = launch_managed_process(ManagedLaunchRequest {
            executable,
            arguments: arguments.collect(),
            current_dir: None,
            environment: std::env::vars_os().collect(),
        })?;
        let slot = process.slot;
        let generation = process.generation;
        let pid = process.identity.pid;
        println!(
            "managed-launch slot={} generation={} pid={}",
            slot, generation, pid
        );
        if std::env::var_os("LTERM_INTERNAL_MANAGED_LAUNCH_NO_WAIT").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            drop(process);
            return Ok(true);
        }
        let terminate = std::env::var_os("LTERM_INTERNAL_MANAGED_LAUNCH_TERMINATE").as_deref()
            == Some(std::ffi::OsStr::new("1"));
        let status = if terminate {
            process.terminate_and_wait()?
        } else {
            process.wait()?
        };
        if !terminate {
            ensure!(
                status.success(),
                "managed root process exited with {status}"
            );
        }
        Ok(true)
    }
}

#[cfg(target_os = "linux")]
fn remap_gate_fds(control_fd: RawFd, guard_fd: RawFd) -> std::io::Result<()> {
    let control_copy = duplicate_for_gate_remap(control_fd)?;
    let guard_copy = match duplicate_for_gate_remap(guard_fd) {
        Ok(fd) => fd,
        Err(err) => {
            unsafe { libc::close(control_copy) };
            return Err(err);
        }
    };
    let result = if unsafe { libc::dup2(control_copy, GATE_CONTROL_FD) } < 0
        || unsafe { libc::dup2(guard_copy, GATE_GUARD_FD) } < 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    };
    unsafe {
        libc::close(control_copy);
        libc::close(guard_copy);
    }
    result
}

#[cfg(target_os = "linux")]
fn duplicate_for_gate_remap(fd: RawFd) -> std::io::Result<RawFd> {
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, GATE_GUARD_FD + 1) };
    if duplicate < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(duplicate)
    }
}

#[cfg(target_os = "linux")]
fn run_gate(arguments: Vec<OsString>) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    ensure!(arguments.len() >= 5, "malformed internal gate invocation");
    let registry_root = PathBuf::from(&arguments[0]);
    ensure!(registry_root.is_absolute());
    let slot: u16 = arguments[1].to_string_lossy().parse()?;
    let generation: u64 = arguments[2].to_string_lossy().parse()?;
    let nonce = Uuid::parse_str(&arguments[3].to_string_lossy())?;
    let target_argv = &arguments[4..];
    ensure!(!target_argv.is_empty());

    let control = unsafe { File::from_raw_fd(GATE_CONTROL_FD) };
    let guard = unsafe { File::from_raw_fd(GATE_GUARD_FD) };
    validate_seqpacket(control.as_raw_fd())?;
    let registry = Registry::open_at(registry_root, SLOT_COUNT)?;
    validate_inherited_guard(&registry, slot, &guard)?;
    let intent = registry.read_valid_slot(slot)?;
    ensure!(intent.generation == generation);
    ensure!(matches!(
        intent.state,
        SlotState::IntentDurable { nonce: value, .. } if value == nonce
    ));
    managed_test_failpoint("gate_before_registration");

    let identity = match observe_identity(std::process::id()) {
        Evidence::Present(identity) => identity,
        Evidence::Absent => bail!("gate cannot observe its own process identity"),
        Evidence::Unavailable(reason) => bail!("gate identity unavailable: {reason}"),
    };
    let registration = GateRegistration {
        schema_version: SCHEMA_VERSION,
        slot,
        generation,
        nonce,
        identity: identity.clone(),
    };
    // Gate self-registration is durable before HELLO. Recovery may therefore
    // identify a busy inherited guard even if the daemon dies before HELLO.
    registry.replace_registration(
        slot,
        &RegistrationRecord {
            schema_version: SCHEMA_VERSION,
            slot,
            generation,
            registration: Some(registration.clone()),
        },
    )?;
    managed_test_failpoint("gate_after_registration");
    send_packet(
        control.as_raw_fd(),
        &GateHello {
            protocol: "lterm-managed-hello-v1".into(),
            registration: registration.clone(),
        },
    )?;
    managed_test_failpoint("gate_after_hello");

    set_socket_timeout(control.as_raw_fd(), Duration::from_secs(30))?;
    let (commit, target) = recv_commit_with_fd(control.as_raw_fd())?;
    managed_test_failpoint("gate_after_commit");
    ensure!(commit.protocol == "lterm-managed-commit-v1");
    ensure!(commit.slot == slot && commit.generation == generation && commit.nonce == nonce);
    ensure!(commit.identity == identity);
    let durable = registry.read_valid_slot(slot)?;
    ensure!(matches!(
        durable.state,
        SlotState::IdentityDurable {
            nonce: value,
            identity: ref durable_identity,
            release_may_have_occurred: true,
        } if value == nonce && durable_identity == &identity
    ));
    ensure!(matches!(
        observe_identity(std::process::id()),
        Evidence::Present(ref actual) if actual == &identity
    ));
    validate_pinned_executable(&target)?;
    set_cloexec(control.as_raw_fd())?;
    set_cloexec(guard.as_raw_fd())?;
    managed_test_failpoint("gate_before_exec");

    let argv = target_argv
        .iter()
        .map(|argument| std::ffi::CString::new(argument.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut argv_ptrs = argv
        .iter()
        .map(|value| value.as_ptr().cast_mut())
        .collect::<Vec<_>>();
    argv_ptrs.push(std::ptr::null_mut());
    let environment = std::env::vars_os()
        .map(|(key, value)| {
            let mut bytes = key.as_bytes().to_vec();
            bytes.push(b'=');
            bytes.extend_from_slice(value.as_bytes());
            std::ffi::CString::new(bytes)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut env_ptrs = environment
        .iter()
        .map(|value| value.as_ptr().cast_mut())
        .collect::<Vec<_>>();
    env_ptrs.push(std::ptr::null_mut());
    let empty = c"";
    let result = unsafe {
        libc::execveat(
            target.as_raw_fd(),
            empty.as_ptr(),
            argv_ptrs.as_ptr(),
            env_ptrs.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        let _ = send_packet(
            control.as_raw_fd(),
            &GateExecFailure {
                protocol: "lterm-managed-exec-failure-v1".into(),
                errno: error.raw_os_error(),
                message: error.to_string(),
            },
        );
        return Err(error).context("execveat failed");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_inherited_guard(registry: &Registry, slot: u16, inherited: &File) -> Result<()> {
    validate_file_handle(inherited, Path::new("inherited slot guard"))?;
    let expected = open_exact_file(&registry.guards.join(guard_name(slot)), false)?;
    let actual_meta = inherited.metadata()?;
    let expected_meta = expected.metadata()?;
    ensure!(actual_meta.dev() == expected_meta.dev());
    ensure!(actual_meta.ino() == expected_meta.ino());
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_pinned_executable(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open pinned target executable {}", path.display()))?;
    validate_pinned_executable(&file)?;
    Ok(file)
}

#[cfg(target_os = "linux")]
fn validate_pinned_executable(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "target executable is not a regular file"
    );
    ensure!(
        metadata.permissions().mode() & 0o111 != 0,
        "target is not executable"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn seqpacket_pair() -> Result<(File, File)> {
    use std::os::fd::FromRawFd;
    let mut fds = [-1; 2];
    let result = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("create SOCK_SEQPACKET pair");
    }
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

#[cfg(target_os = "linux")]
fn validate_seqpacket(fd: RawFd) -> Result<()> {
    let mut kind = 0i32;
    let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut i32).cast(),
            &mut length,
        )
    };
    ensure!(result == 0, "SO_TYPE unavailable");
    ensure!(
        kind == libc::SOCK_SEQPACKET,
        "control FD is not SOCK_SEQPACKET"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_socket_timeout(fd: RawFd, timeout: Duration) -> Result<()> {
    let value = libc::timeval {
        tv_sec: timeout.as_secs().try_into().unwrap_or(libc::time_t::MAX),
        tv_usec: timeout.subsec_micros().into(),
    };
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&value as *const libc::timeval).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("set gate receive timeout")
    }
}

#[cfg(target_os = "linux")]
fn send_packet<T: Serialize>(fd: RawFd, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_RECORD_BYTES);
    let sent = unsafe { libc::send(fd, bytes.as_ptr().cast(), bytes.len(), libc::MSG_NOSIGNAL) };
    ensure!(
        sent == bytes.len() as isize,
        "short or failed seqpacket send"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn recv_packet<T: for<'de> Deserialize<'de>>(fd: RawFd) -> Result<T> {
    let mut bytes = [0u8; MAX_RECORD_BYTES + 1];
    let received =
        unsafe { libc::recv(fd, bytes.as_mut_ptr().cast(), bytes.len(), libc::MSG_TRUNC) };
    ensure!(received > 0, "gate control EOF or receive failure");
    ensure!(
        received as usize <= MAX_RECORD_BYTES,
        "oversized gate packet"
    );
    serde_json::from_slice(&bytes[..received as usize]).context("malformed gate packet")
}

#[cfg(target_os = "linux")]
fn send_commit_with_fd(fd: RawFd, commit: &GateCommit, target_fd: RawFd) -> Result<()> {
    let bytes = serde_json::to_vec(commit)?;
    ensure!(bytes.len() <= MAX_RECORD_BYTES);
    let mut iovec = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let control_len = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; control_len];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        ensure!(!header.is_null());
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize;
        std::ptr::write(libc::CMSG_DATA(header).cast::<RawFd>(), target_fd);
        message.msg_controllen = (*header).cmsg_len;
    }
    let sent = unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL) };
    ensure!(sent == bytes.len() as isize, "atomic COMMIT send failed");
    Ok(())
}

#[cfg(target_os = "linux")]
fn recv_commit_with_fd(fd: RawFd) -> Result<(GateCommit, File)> {
    use std::os::fd::FromRawFd;
    let mut bytes = [0u8; MAX_RECORD_BYTES + 1];
    let mut iovec = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let control_len = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; control_len];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    let received =
        unsafe { libc::recvmsg(fd, &mut message, libc::MSG_CMSG_CLOEXEC | libc::MSG_TRUNC) };
    ensure!(received > 0, "COMMIT EOF or receive failure");
    ensure!(received as usize <= MAX_RECORD_BYTES, "oversized COMMIT");
    ensure!(message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0);
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    ensure!(!header.is_null(), "COMMIT has no target executable FD");
    unsafe {
        ensure!((*header).cmsg_level == libc::SOL_SOCKET);
        ensure!((*header).cmsg_type == libc::SCM_RIGHTS);
        ensure!((*header).cmsg_len == libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize);
    }
    let target_fd = unsafe { std::ptr::read(libc::CMSG_DATA(header).cast::<RawFd>()) };
    ensure!(target_fd >= 0);
    let commit = serde_json::from_slice(&bytes[..received as usize]).context("malformed COMMIT")?;
    Ok((commit, unsafe { File::from_raw_fd(target_fd) }))
}

#[cfg(target_os = "linux")]
fn set_cloexec(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    ensure!(flags >= 0);
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    ensure!(result == 0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn registry(slot_count: usize) -> (TempDir, Registry) {
        let temp = TempDir::new().expect("tempdir");
        let parent = temp.path().join("speculation");
        create_exact_dir(&parent).expect("private parent");
        let registry = Registry::open_at(parent.join("process-registry-v1"), slot_count)
            .expect("registry genesis");
        (temp, registry)
    }

    fn identity(pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            boot_uuid: Uuid::from_u128(1),
            pid_namespace_inode: 2,
            pid,
            start_ticks: 3,
        }
    }

    #[test]
    fn genesis_creates_fixed_exact_layout_and_reopens() {
        let (_temp, registry) = registry(4);
        registry.validate_layout().expect("valid layout");
        assert_eq!(fs::read_dir(&registry.slots).unwrap().count(), 4);
        assert_eq!(fs::read_dir(&registry.guards).unwrap().count(), 4);
        assert_eq!(fs::read_dir(&registry.registrations).unwrap().count(), 4);
        let reopened = Registry::open_at(registry.root.clone(), 4).expect("reopen");
        assert_eq!(
            reopened.read_valid_slot(0).unwrap().state,
            SlotState::Vacant
        );
    }

    #[test]
    fn symlink_or_wrong_mode_fixed_file_fails_closed() {
        use std::os::unix::fs::symlink;
        let (_temp, registry) = registry(2);
        let slot = registry.slots.join(slot_name(0));
        fs::remove_file(&slot).unwrap();
        symlink(registry.slots.join(slot_name(1)), &slot).unwrap();
        assert!(registry.validate_layout().is_err());

        fs::remove_file(&slot).unwrap();
        create_exact_json_file(
            &slot,
            &SlotRecord {
                schema_version: SCHEMA_VERSION,
                slot: 0,
                generation: 0,
                state: SlotState::Vacant,
            },
        )
        .unwrap();
        fs::set_permissions(&slot, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(registry.validate_layout().is_err());
    }

    #[test]
    fn corrupt_and_oversized_records_are_unknown_not_absent() {
        let (_temp, registry) = registry(2);
        let slot = registry.slots.join(slot_name(0));
        fs::write(&slot, b"not json").unwrap();
        fs::set_permissions(&slot, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(registry.read_slot(0), SlotRead::Unknown(_)));

        let slot = registry.slots.join(slot_name(1));
        fs::write(&slot, vec![b'x'; MAX_RECORD_BYTES + 1]).unwrap();
        fs::set_permissions(&slot, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(registry.read_slot(1), SlotRead::Unknown(_)));
    }

    #[test]
    fn legal_state_machine_rejects_shortcuts_and_generation_wrap() {
        let nonce = Uuid::from_u128(9);
        let vacant = SlotRecord {
            schema_version: SCHEMA_VERSION,
            slot: 0,
            generation: 4,
            state: SlotState::Vacant,
        };
        let intent = SlotRecord {
            generation: 5,
            state: SlotState::IntentDurable {
                nonce,
                created_unix_secs: 1,
            },
            ..vacant.clone()
        };
        assert!(validate_transition(&vacant, &intent).is_ok());
        let cleanup = SlotRecord {
            state: SlotState::CleanupPending {
                nonce,
                identity: identity(10),
                release_may_have_occurred: true,
            },
            ..intent.clone()
        };
        assert!(validate_transition(&intent, &cleanup).is_err());
        let exhausted = SlotRecord {
            generation: u64::MAX,
            ..vacant.clone()
        };
        let wrapped = SlotRecord {
            generation: 0,
            state: intent.state,
            ..exhausted.clone()
        };
        assert!(validate_transition(&exhausted, &wrapped).is_err());
    }

    #[test]
    fn registration_is_bound_to_slot_generation_and_nonce() {
        let (_temp, registry) = registry(1);
        let registration = GateRegistration {
            schema_version: SCHEMA_VERSION,
            slot: 0,
            generation: 7,
            nonce: Uuid::from_u128(7),
            identity: identity(42),
        };
        registry
            .replace_registration(
                0,
                &RegistrationRecord {
                    schema_version: SCHEMA_VERSION,
                    slot: 0,
                    generation: 7,
                    registration: Some(registration.clone()),
                },
            )
            .unwrap();
        let readback = registry.read_registration(0).unwrap();
        assert_eq!(readback.registration, Some(registration));
    }

    #[test]
    fn proc_stat_parser_handles_closing_parentheses_inside_command() {
        let mut fields = vec!["S".to_string()];
        fields.extend((4..=21).map(|value| value.to_string()));
        fields.push("987654".into());
        fields.push("23".into());
        let stat = format!("123 (worker ) name) {}", fields.join(" "));
        assert_eq!(parse_proc_stat_start_ticks(&stat).unwrap(), 987654);
        assert!(parse_proc_stat_start_ticks("123 broken").is_err());
    }

    #[test]
    fn non_linux_launch_fails_closed() {
        #[cfg(not(target_os = "linux"))]
        {
            let result = launch_managed_process(ManagedLaunchRequest {
                executable: PathBuf::from("/bin/echo"),
                arguments: Vec::new(),
                current_dir: None,
                environment: Vec::new(),
            });
            assert!(result.unwrap_err().to_string().contains("only on Linux"));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn separate_ofd_descriptions_contend() {
        let (_temp, registry) = registry(1);
        let first = OfdLock::try_acquire(&registry.guards.join(guard_name(0))).unwrap();
        assert!(OfdLock::try_acquire(&registry.guards.join(guard_name(0))).is_err());
        drop(first);
        OfdLock::try_acquire(&registry.guards.join(guard_name(0))).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fixed_capacity_fails_before_reusing_unresolved_records() {
        let (_temp, registry) = registry(2);
        let first = registry.allocate_intent(1).unwrap();
        let second = registry.allocate_intent(1).unwrap();
        assert!(registry.allocate_intent(1).is_err());
        drop((first, second));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn full_fixed_capacity_refuses_before_creating_another_intent() {
        let (_temp, registry) = registry(SLOT_COUNT);
        for slot in 0..SLOT_COUNT {
            let slot = u16::try_from(slot).unwrap();
            let vacant = registry.read_valid_slot(slot).unwrap();
            registry
                .replace_slot(
                    &vacant,
                    &SlotRecord {
                        generation: 1,
                        state: SlotState::IntentDurable {
                            nonce: Uuid::new_v4(),
                            created_unix_secs: 1,
                        },
                        ..vacant.clone()
                    },
                )
                .unwrap();
        }
        let error = registry.allocate_intent(2).unwrap_err().to_string();
        assert!(error.contains("1024/1024 unresolved"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unlocked_intent_is_durably_tombstoned() {
        let (_temp, registry) = registry(1);
        let intent = registry.allocate_intent(1).unwrap();
        let slot = intent.record.slot;
        let generation = intent.record.generation;
        let SlotState::IntentDurable { nonce, .. } = intent.record.state else {
            unreachable!()
        };
        drop(intent);

        assert_eq!(
            registry.cleanup(slot, generation).unwrap(),
            ReconcileOutcome::ResolvedTombstone
        );
        assert!(matches!(
            registry.read_valid_slot(slot).unwrap().state,
            SlotState::ResolvedTombstone {
                nonce: value,
                identity: None,
                ..
            } if value == nonce
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn busy_intent_with_absent_registered_identity_stays_unknown() {
        let (_temp, registry) = registry(1);
        let intent = registry.allocate_intent(1).unwrap();
        let SlotState::IntentDurable { nonce, .. } = intent.record.state else {
            unreachable!()
        };
        registry
            .replace_registration(
                intent.record.slot,
                &RegistrationRecord {
                    schema_version: SCHEMA_VERSION,
                    slot: intent.record.slot,
                    generation: intent.record.generation,
                    registration: Some(GateRegistration {
                        schema_version: SCHEMA_VERSION,
                        slot: intent.record.slot,
                        generation: intent.record.generation,
                        nonce,
                        identity: identity(u32::MAX),
                    }),
                },
            )
            .unwrap();

        assert!(matches!(
            registry
                .cleanup(intent.record.slot, intent.record.generation)
                .unwrap(),
            ReconcileOutcome::UnknownOrphanRisk(reason)
                if reason.contains("busy intent guard")
        ));
        assert!(matches!(
            registry.read_valid_slot(intent.record.slot).unwrap().state,
            SlotState::IntentDurable { .. }
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn current_process_identity_is_typed_and_pidfd_verified() {
        let observed = observe_identity(std::process::id());
        let Evidence::Present(identity) = observed else {
            panic!("current identity unavailable: {observed:?}");
        };
        assert!(matches!(
            open_verified_pidfd(&identity),
            Evidence::Present(_)
        ));
        let reused = ProcessIdentity {
            start_ticks: identity.start_ticks.saturating_add(1),
            ..identity
        };
        assert!(matches!(open_verified_pidfd(&reused), Evidence::Absent));
    }
}
