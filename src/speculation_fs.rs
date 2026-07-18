use serde::Serialize;
use serde::de::DeserializeOwned;
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use uuid::Uuid;

pub const MAX_SPECULATION_JSON_BYTES: usize = 32 * 1024;
const MAX_DIRECTORY_PATH_BYTES: usize = 4096;
const MAX_LEAF_BYTES: usize = 255;
const TRANSACTION_TEMP_PREFIX: &str = ".lterm-speculation-txn-v1-";
const TRANSACTION_TEMP_SUFFIX: &str = ".tmp";

#[cfg(all(target_os = "linux", test))]
std::thread_local! {
    static FORCE_DIRECT_LINK_ENOENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceError {
    Unsupported,
    InvalidDirectory,
    InvalidIdentity,
    Overlap,
    InvalidLeaf,
    AlreadyExists,
    Missing,
    TooLarge,
    Corrupt,
    Stale,
    Io,
    Capacity,
    GenerationMismatch,
    Poisoned,
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = match self {
            Self::Unsupported => "speculation_evidence_unsupported",
            Self::InvalidDirectory => "speculation_invalid_directory",
            Self::InvalidIdentity => "speculation_invalid_identity",
            Self::Overlap => "speculation_root_overlap",
            Self::InvalidLeaf => "speculation_invalid_leaf",
            Self::AlreadyExists => "speculation_record_exists",
            Self::Missing => "speculation_record_missing",
            Self::TooLarge => "speculation_record_too_large",
            Self::Corrupt => "speculation_record_corrupt",
            Self::Stale => "speculation_record_stale",
            Self::Io => "speculation_evidence_io",
            Self::Capacity => "speculation_capacity_exhausted",
            Self::GenerationMismatch => "speculation_generation_mismatch",
            Self::Poisoned => "speculation_evidence_poisoned",
        };
        formatter.write_str(code)
    }
}

impl std::error::Error for EvidenceError {}

pub type EvidenceResult<T> = Result<T, EvidenceError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableDirectoryIdentity {
    pub boot_uuid: Uuid,
    pub dev: u64,
    pub ino: u64,
    pub statx_mnt_id_unique: u64,
}

/// Same-incarnation identity that is valid only while the original directory
/// handle remains open.  Ordinary `STATX_MNT_ID` is deliberately not
/// serializable and must never authorize restart recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveMountIdentity {
    pub dev: u64,
    pub ino: u64,
    pub statx_mnt_id: u64,
}

#[cfg(test)]
impl DurableDirectoryIdentity {
    pub fn test_value() -> Self {
        Self {
            boot_uuid: Uuid::from_u128(1),
            dev: 2,
            ino: 3,
            statx_mnt_id_unique: 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

pub struct ValidatedDirectory {
    file: File,
    identity: DurableDirectoryIdentity,
    canonical_locator: PathBuf,
    policy: DirectoryPolicy,
    mutation_lock: Mutex<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectoryPolicy {
    Private,
    Workspace,
    DelegatedCgroup,
}

impl fmt::Debug for ValidatedDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedDirectory")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl ValidatedDirectory {
    pub fn identity(&self) -> DurableDirectoryIdentity {
        self.identity
    }

    pub fn revalidate(&self) -> EvidenceResult<()> {
        let current = directory_identity(&self.file)?;
        if current != self.identity {
            return Err(EvidenceError::Stale);
        }
        validate_directory_policy(&self.file, self.policy)?;
        self.reopen_and_verify()
    }

    pub fn reopen_and_verify(&self) -> EvidenceResult<()> {
        let reopened = open_directory(&self.canonical_locator, false, self.policy)?;
        if reopened.identity != self.identity {
            return Err(EvidenceError::Stale);
        }
        Ok(())
    }

    pub(crate) fn try_clone_retained_fd(&self) -> EvidenceResult<File> {
        clone_cloexec(&self.file)
    }

    pub(crate) fn live_mount_identity(&self) -> EvidenceResult<LiveMountIdentity> {
        live_mount_identity_from_fd(&self.file)
    }

    pub(crate) fn canonical_locator_bytes(&self) -> &[u8] {
        self.canonical_locator.as_os_str().as_bytes()
    }

    pub(crate) fn list_leaf_names(&self) -> EvidenceResult<Vec<CString>> {
        let _guard = self.lock_mutations()?;
        self.revalidate()?;
        let mut names = enumerate_leaf_names_from_fd(&self.file, self.identity)?;
        let mut removed_transaction_temp = false;
        names.retain(|name| {
            if !is_transaction_temp_leaf(name) {
                return true;
            }
            if validate_temp_leaf_for_recovery(self.file.as_raw_fd(), name).is_ok()
                && unlinkat_checked(self.file.as_raw_fd(), name).is_ok()
            {
                removed_transaction_temp = true;
                return false;
            }
            true
        });
        if removed_transaction_temp {
            self.file.sync_all().map_err(|_| EvidenceError::Io)?;
        }
        self.revalidate()?;
        Ok(names)
    }

    fn lock_mutations(&self) -> EvidenceResult<DirectoryMutationGuard<'_>> {
        let local = self
            .mutation_lock
            .lock()
            .map_err(|_| EvidenceError::Poisoned)?;
        lock_directory(self.file.as_raw_fd())?;
        Ok(DirectoryMutationGuard {
            directory: self,
            _local: local,
        })
    }
}

struct DirectoryMutationGuard<'a> {
    directory: &'a ValidatedDirectory,
    _local: MutexGuard<'a, ()>,
}

impl Drop for DirectoryMutationGuard<'_> {
    fn drop(&mut self) {
        unlock_directory(self.directory.file.as_raw_fd());
    }
}

pub fn open_existing_private_dir(path: &Path) -> EvidenceResult<ValidatedDirectory> {
    open_directory(path, false, DirectoryPolicy::Private)
}

pub fn open_or_create_private_dir(path: &Path) -> EvidenceResult<ValidatedDirectory> {
    open_directory(path, true, DirectoryPolicy::Private)
}

/// Opens SOURCE or candidate workspace roots component-by-component without
/// following symlinks.  Recursive entry validation remains the containment
/// adapter's responsibility because it is an action-specific bounded scan.
pub fn open_existing_workspace_dir(path: &Path) -> EvidenceResult<ValidatedDirectory> {
    open_directory(path, false, DirectoryPolicy::Workspace)
}

/// Opens an already delegated cgroup-v2 domain root without requiring private
/// directory mode.  The root must be controlled by the current euid, not
/// group/other writable, task-free, and expose the required controller files.
pub fn open_existing_delegated_cgroup_root(path: &Path) -> EvidenceResult<ValidatedDirectory> {
    open_directory(path, false, DirectoryPolicy::DelegatedCgroup)
}

#[cfg(target_os = "linux")]
fn open_directory(
    path: &Path,
    create: bool,
    policy: DirectoryPolicy,
) -> EvidenceResult<ValidatedDirectory> {
    if create && policy != DirectoryPolicy::Private {
        return Err(EvidenceError::InvalidDirectory);
    }
    validate_absolute_path(path)?;
    let canonical = if create {
        create_and_open_components(path)?
    } else {
        open_components(path, false)?
    };
    let canonical_path =
        std::fs::canonicalize(path).map_err(|_| EvidenceError::InvalidDirectory)?;
    if canonical_path != path {
        return Err(EvidenceError::InvalidDirectory);
    }
    validate_directory_policy(&canonical, policy)?;
    let identity = directory_identity(&canonical)?;
    Ok(ValidatedDirectory {
        file: canonical,
        identity,
        canonical_locator: canonical_path,
        policy,
        mutation_lock: Mutex::new(()),
    })
}

#[cfg(not(target_os = "linux"))]
fn open_directory(
    _path: &Path,
    _create: bool,
    _policy: DirectoryPolicy,
) -> EvidenceResult<ValidatedDirectory> {
    Err(EvidenceError::Unsupported)
}

pub fn validate_no_overlap(directories: &[&ValidatedDirectory]) -> EvidenceResult<()> {
    for (index, left) in directories.iter().enumerate() {
        for right in &directories[index + 1..] {
            let same_inode =
                left.identity.dev == right.identity.dev && left.identity.ino == right.identity.ino;
            let nested = left.canonical_locator.starts_with(&right.canonical_locator)
                || right.canonical_locator.starts_with(&left.canonical_locator);
            if same_inode || nested {
                return Err(EvidenceError::Overlap);
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct StoredJson<T> {
    pub value: T,
    pub identity: FileIdentity,
}

pub fn read_json<T: DeserializeOwned>(
    directory: &ValidatedDirectory,
    leaf: &CStr,
    cap: usize,
) -> EvidenceResult<StoredJson<T>> {
    directory.revalidate()?;
    validate_leaf(leaf)?;
    let mut file = openat_file(
        directory.file.as_raw_fd(),
        leaf,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    let identity = validate_record_file(&file)?;
    let bytes = read_bounded(&mut file, cap)?;
    let value = serde_json::from_slice(&bytes).map_err(|_| EvidenceError::Corrupt)?;
    Ok(StoredJson { value, identity })
}

pub fn atomic_create_json<T>(
    directory: &ValidatedDirectory,
    leaf: &CStr,
    value: &T,
    cap: usize,
) -> EvidenceResult<StoredJson<T>>
where
    T: Serialize + DeserializeOwned,
{
    validate_leaf(leaf)?;
    let bytes = serialize_bounded(value, cap)?;
    let _guard = directory.lock_mutations()?;
    directory.revalidate()?;
    let mut file = create_unnamed_temp(directory.file.as_raw_fd())?;
    write_and_sync(&mut file, &bytes)?;
    let unlinked_identity = validate_temporary_record_file(&file)?;
    publish_unnamed_no_replace(&file, directory.file.as_raw_fd(), leaf)?;
    let published_identity = validate_record_file(&file)?;
    if published_identity != unlinked_identity {
        unlinkat(directory.file.as_raw_fd(), leaf);
        return Err(EvidenceError::Stale);
    }
    directory.file.sync_all().map_err(|_| EvidenceError::Io)?;
    let stored = read_exact_written_json(directory, leaf, published_identity, &bytes, cap)?;
    directory.revalidate()?;
    Ok(stored)
}

pub fn atomic_replace_json<T>(
    directory: &ValidatedDirectory,
    leaf: &CStr,
    expected: FileIdentity,
    value: &T,
    cap: usize,
) -> EvidenceResult<StoredJson<T>>
where
    T: Serialize + DeserializeOwned,
{
    validate_leaf(leaf)?;
    let bytes = serialize_bounded(value, cap)?;
    let _guard = directory.lock_mutations()?;
    directory.revalidate()?;
    let current = openat_file(
        directory.file.as_raw_fd(),
        leaf,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    if validate_record_file(&current)? != expected {
        return Err(EvidenceError::Stale);
    }

    let mut file = create_unnamed_temp(directory.file.as_raw_fd())?;
    write_and_sync(&mut file, &bytes)?;
    let unlinked_identity = validate_temporary_record_file(&file)?;
    let temp = transaction_temp_leaf();
    let result = (|| {
        publish_unnamed_no_replace(&file, directory.file.as_raw_fd(), &temp)?;
        let published_identity = validate_record_file(&file)?;
        if published_identity != unlinked_identity {
            return Err(EvidenceError::Stale);
        }
        renameat(directory.file.as_raw_fd(), &temp, leaf)?;
        directory.file.sync_all().map_err(|_| EvidenceError::Io)?;
        let stored = read_exact_written_json(directory, leaf, published_identity, &bytes, cap)?;
        directory.revalidate()?;
        Ok(stored)
    })();
    if result.is_err() {
        unlinkat(directory.file.as_raw_fd(), &temp);
    }
    result
}

fn read_exact_written_json<T: DeserializeOwned>(
    directory: &ValidatedDirectory,
    leaf: &CStr,
    expected_identity: FileIdentity,
    expected_bytes: &[u8],
    cap: usize,
) -> EvidenceResult<StoredJson<T>> {
    let mut file = openat_file(
        directory.file.as_raw_fd(),
        leaf,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    let identity = validate_record_file(&file)?;
    if identity != expected_identity {
        return Err(EvidenceError::Stale);
    }
    let bytes = read_bounded(&mut file, cap)?;
    if bytes != expected_bytes {
        return Err(EvidenceError::Corrupt);
    }
    let value = serde_json::from_slice(&bytes).map_err(|_| EvidenceError::Corrupt)?;
    Ok(StoredJson { value, identity })
}

fn serialize_bounded<T: Serialize>(value: &T, cap: usize) -> EvidenceResult<Vec<u8>> {
    let effective_cap = cap.min(MAX_SPECULATION_JSON_BYTES);
    let bytes = serde_json::to_vec(value).map_err(|_| EvidenceError::Corrupt)?;
    if bytes.len() > effective_cap {
        return Err(EvidenceError::TooLarge);
    }
    Ok(bytes)
}

fn read_bounded(file: &mut File, cap: usize) -> EvidenceResult<Vec<u8>> {
    let effective_cap = cap.min(MAX_SPECULATION_JSON_BYTES);
    let mut bytes = Vec::with_capacity(effective_cap.min(4096));
    file.take(effective_cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| EvidenceError::Io)?;
    if bytes.len() > effective_cap {
        return Err(EvidenceError::TooLarge);
    }
    Ok(bytes)
}

fn write_and_sync(file: &mut File, bytes: &[u8]) -> EvidenceResult<()> {
    file.write_all(bytes).map_err(|_| EvidenceError::Io)?;
    file.sync_all().map_err(|_| EvidenceError::Io)
}

fn validate_record_file(file: &File) -> EvidenceResult<FileIdentity> {
    validate_regular_file(file, 1)
}

fn validate_temporary_record_file(file: &File) -> EvidenceResult<FileIdentity> {
    validate_regular_file(file, 0)
}

fn validate_regular_file(file: &File, expected_links: u64) -> EvidenceResult<FileIdentity> {
    let metadata = file.metadata().map_err(|_| EvidenceError::Io)?;
    let file_type = metadata.file_type();
    if !file_type.is_file()
        || file_type.is_symlink()
        || file_type.is_fifo()
        || file_type.is_socket()
        || file_type.is_block_device()
        || file_type.is_char_device()
        || metadata.uid() != current_euid()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != expected_links
    {
        return Err(EvidenceError::InvalidIdentity);
    }
    Ok(FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

fn validate_private_directory_metadata(file: &File) -> EvidenceResult<()> {
    let metadata = file.metadata().map_err(|_| EvidenceError::Io)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != current_euid()
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(EvidenceError::InvalidDirectory);
    }
    Ok(())
}

fn validate_workspace_directory_metadata(file: &File) -> EvidenceResult<()> {
    let metadata = file.metadata().map_err(|_| EvidenceError::Io)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != current_euid()
        || metadata.mode() & 0o6000 != 0
        || metadata.mode() & 0o022 != 0
    {
        return Err(EvidenceError::InvalidDirectory);
    }
    Ok(())
}

fn validate_directory_policy(file: &File, policy: DirectoryPolicy) -> EvidenceResult<()> {
    match policy {
        DirectoryPolicy::Private => validate_private_directory_metadata(file),
        DirectoryPolicy::Workspace => validate_workspace_directory_metadata(file),
        DirectoryPolicy::DelegatedCgroup => validate_delegated_cgroup_root(file),
    }
}

#[cfg(target_os = "linux")]
fn validate_delegated_cgroup_root(file: &File) -> EvidenceResult<()> {
    let metadata = file.metadata().map_err(|_| EvidenceError::Io)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != current_euid()
        || metadata.mode() & 0o022 != 0
    {
        return Err(EvidenceError::InvalidDirectory);
    }
    let mut statfs = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::fstatfs(file.as_raw_fd(), statfs.as_mut_ptr()) } != 0 {
        return Err(EvidenceError::Io);
    }
    let statfs = unsafe { statfs.assume_init() };
    if statfs.f_type as u64 != libc::CGROUP2_SUPER_MAGIC as u64 {
        return Err(EvidenceError::InvalidDirectory);
    }
    let cgroup_type = read_small_leaf(file.as_raw_fd(), c"cgroup.type", 64)?;
    if cgroup_type != b"domain\n" {
        return Err(EvidenceError::InvalidDirectory);
    }
    if !read_small_leaf(file.as_raw_fd(), c"cgroup.procs", 4096)?.is_empty() {
        return Err(EvidenceError::InvalidDirectory);
    }
    let controllers = read_small_leaf(file.as_raw_fd(), c"cgroup.controllers", 4096)?;
    if !controllers
        .split(|byte| byte.is_ascii_whitespace())
        .any(|controller| controller == b"pids")
    {
        return Err(EvidenceError::InvalidDirectory);
    }
    for (leaf, flags) in [
        (c"cgroup.kill", libc::O_WRONLY),
        (c"cgroup.events", libc::O_RDONLY),
        (c"cgroup.subtree_control", libc::O_RDWR),
    ] {
        let opened = openat_file(
            file.as_raw_fd(),
            leaf,
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )?;
        if !opened.metadata().map_err(|_| EvidenceError::Io)?.is_file() {
            return Err(EvidenceError::InvalidDirectory);
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_delegated_cgroup_root(_file: &File) -> EvidenceResult<()> {
    Err(EvidenceError::Unsupported)
}

fn read_small_leaf(dirfd: RawFd, leaf: &CStr, cap: usize) -> EvidenceResult<Vec<u8>> {
    let file = openat_file(
        dirfd,
        leaf,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    let mut bytes = Vec::with_capacity(cap.min(4096));
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| EvidenceError::Io)?;
    if bytes.len() > cap {
        return Err(EvidenceError::TooLarge);
    }
    Ok(bytes)
}

fn validate_absolute_path(path: &Path) -> EvidenceResult<()> {
    if !path.is_absolute()
        || path.as_os_str().as_bytes().len() > MAX_DIRECTORY_PATH_BYTES
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(EvidenceError::InvalidDirectory);
    }
    Ok(())
}

fn validate_leaf(leaf: &CStr) -> EvidenceResult<()> {
    let bytes = leaf.to_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_LEAF_BYTES
        || bytes == b"."
        || bytes == b".."
        || bytes.contains(&b'/')
    {
        return Err(EvidenceError::InvalidLeaf);
    }
    Ok(())
}

fn transaction_temp_leaf() -> CString {
    CString::new(format!(
        "{TRANSACTION_TEMP_PREFIX}{}{TRANSACTION_TEMP_SUFFIX}",
        Uuid::new_v4()
    ))
    .expect("transaction temp leaf is NUL-free")
}

fn is_transaction_temp_leaf(leaf: &CStr) -> bool {
    let Ok(value) = std::str::from_utf8(leaf.to_bytes()) else {
        return false;
    };
    let Some(uuid) = value
        .strip_prefix(TRANSACTION_TEMP_PREFIX)
        .and_then(|value| value.strip_suffix(TRANSACTION_TEMP_SUFFIX))
    else {
        return false;
    };
    Uuid::parse_str(uuid).is_ok()
}

fn openat_file(dirfd: RawFd, leaf: &CStr, flags: i32, mode: u32) -> EvidenceResult<File> {
    let fd = unsafe { libc::openat(dirfd, leaf.as_ptr(), flags, mode as libc::c_uint) };
    if fd < 0 {
        return Err(map_io_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn clone_cloexec(file: &File) -> EvidenceResult<File> {
    let duplicate = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(EvidenceError::Io);
    }
    Ok(unsafe { File::from_raw_fd(duplicate) })
}

fn renameat(dirfd: RawFd, from: &CStr, to: &CStr) -> EvidenceResult<()> {
    if unsafe { libc::renameat(dirfd, from.as_ptr(), dirfd, to.as_ptr()) } != 0 {
        return Err(map_io_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn create_unnamed_temp(dirfd: RawFd) -> EvidenceResult<File> {
    let fd = unsafe {
        libc::openat(
            dirfd,
            c".".as_ptr(),
            libc::O_WRONLY | libc::O_TMPFILE | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EOPNOTSUPP | libc::EINVAL | libc::ENOSYS) => Err(EvidenceError::Unsupported),
            _ => Err(EvidenceError::Io),
        };
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(not(target_os = "linux"))]
fn create_unnamed_temp(_dirfd: RawFd) -> EvidenceResult<File> {
    Err(EvidenceError::Unsupported)
}

#[cfg(target_os = "linux")]
fn direct_link_unnamed(file: &File, dirfd: RawFd, leaf: &CStr) -> i32 {
    #[cfg(test)]
    if FORCE_DIRECT_LINK_ENOENT.with(std::cell::Cell::get) {
        unsafe {
            *libc::__errno_location() = libc::ENOENT;
        }
        return -1;
    }

    // Linux open(2)/linkat(2) explicitly permit O_TMPFILE without O_EXCL to be
    // linked with AT_EMPTY_PATH even when CAP_DAC_READ_SEARCH is absent.
    unsafe {
        libc::linkat(
            file.as_raw_fd(),
            c"".as_ptr(),
            dirfd,
            leaf.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    }
}

#[cfg(target_os = "linux")]
fn publish_unnamed_no_replace(file: &File, dirfd: RawFd, leaf: &CStr) -> EvidenceResult<()> {
    let mut result = direct_link_unnamed(file, dirfd, leaf);
    let direct_error = if result != 0 {
        std::io::Error::last_os_error().raw_os_error()
    } else {
        None
    };
    if matches!(direct_error, Some(libc::EPERM | libc::ENOENT)) {
        let proc_fd = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .map_err(|_| EvidenceError::InvalidLeaf)?;
        result = unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                proc_fd.as_ptr(),
                dirfd,
                leaf.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        };
    }
    if result != 0 {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EEXIST) => Err(EvidenceError::AlreadyExists),
            Some(
                libc::EOPNOTSUPP
                | libc::EINVAL
                | libc::ENOSYS
                | libc::EPERM
                | libc::EACCES
                | libc::ENOENT
                | libc::EXDEV,
            ) => Err(EvidenceError::Unsupported),
            _ => Err(EvidenceError::Io),
        };
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn publish_unnamed_no_replace(_file: &File, _dirfd: RawFd, _leaf: &CStr) -> EvidenceResult<()> {
    Err(EvidenceError::Unsupported)
}

fn validate_temp_leaf_for_recovery(dirfd: RawFd, leaf: &CStr) -> EvidenceResult<()> {
    if !is_transaction_temp_leaf(leaf) {
        return Err(EvidenceError::InvalidLeaf);
    }
    let mut file = openat_file(
        dirfd,
        leaf,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    validate_record_file(&file)?;
    let bytes = read_bounded(&mut file, MAX_SPECULATION_JSON_BYTES)?;
    serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|_| EvidenceError::Corrupt)?;
    Ok(())
}

fn unlinkat(dirfd: RawFd, leaf: &CStr) {
    unsafe {
        libc::unlinkat(dirfd, leaf.as_ptr(), 0);
    }
}

fn unlinkat_checked(dirfd: RawFd, leaf: &CStr) -> EvidenceResult<()> {
    if unsafe { libc::unlinkat(dirfd, leaf.as_ptr(), 0) } != 0 {
        return Err(map_io_error());
    }
    Ok(())
}

fn map_io_error() -> EvidenceError {
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EEXIST) => EvidenceError::AlreadyExists,
        Some(libc::ENOENT) => EvidenceError::Missing,
        _ => EvidenceError::Io,
    }
}

fn current_euid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(target_os = "linux")]
fn lock_directory(fd: RawFd) -> EvidenceResult<()> {
    if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
        return Err(EvidenceError::Io);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn lock_directory(_fd: RawFd) -> EvidenceResult<()> {
    Err(EvidenceError::Unsupported)
}

#[cfg(target_os = "linux")]
fn unlock_directory(fd: RawFd) {
    unsafe {
        libc::flock(fd, libc::LOCK_UN);
    }
}

#[cfg(not(target_os = "linux"))]
fn unlock_directory(_fd: RawFd) {}

#[cfg(target_os = "linux")]
fn enumerate_leaf_names_from_fd(
    directory: &File,
    expected_identity: DurableDirectoryIdentity,
) -> EvidenceResult<Vec<CString>> {
    if directory_identity(directory)? != expected_identity {
        return Err(EvidenceError::Stale);
    }
    validate_private_directory_metadata(directory)?;
    let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(EvidenceError::Io);
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe {
            libc::close(duplicate);
        }
        return Err(EvidenceError::Io);
    }
    let mut names = Vec::new();
    loop {
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let errno = unsafe { *libc::__errno_location() };
            unsafe {
                libc::closedir(stream);
            }
            if errno != 0 {
                return Err(EvidenceError::Io);
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(CString::new(name.to_bytes()).map_err(|_| EvidenceError::InvalidLeaf)?);
        }
    }
    if directory_identity(directory)? != expected_identity {
        return Err(EvidenceError::Stale);
    }
    Ok(names)
}

#[cfg(not(target_os = "linux"))]
fn enumerate_leaf_names_from_fd(
    _directory: &File,
    _expected_identity: DurableDirectoryIdentity,
) -> EvidenceResult<Vec<CString>> {
    Err(EvidenceError::Unsupported)
}

#[cfg(target_os = "linux")]
fn create_and_open_components(path: &Path) -> EvidenceResult<File> {
    open_components(path, true)
}

#[cfg(target_os = "linux")]
fn open_components(path: &Path, create: bool) -> EvidenceResult<File> {
    let root_fd = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        return Err(EvidenceError::Io);
    }
    let mut current = unsafe { File::from_raw_fd(root_fd) };
    let mut saw_normal = false;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        saw_normal = true;
        let name = CString::new(name.as_bytes()).map_err(|_| EvidenceError::InvalidDirectory)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let mut fd = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 && create && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT)
        {
            if unsafe { libc::mkdirat(current.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                return Err(map_io_error());
            }
            fd = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
        }
        if fd < 0 {
            return Err(EvidenceError::InvalidDirectory);
        }
        current = unsafe { File::from_raw_fd(fd) };
    }
    if !saw_normal {
        return Err(EvidenceError::InvalidDirectory);
    }
    Ok(current)
}

#[cfg(target_os = "linux")]
pub(crate) fn durable_identity_from_fd(file: &File) -> EvidenceResult<DurableDirectoryIdentity> {
    const STATX_MNT_ID_UNIQUE: u32 = 0x0000_4000;
    let metadata = file.metadata().map_err(|_| EvidenceError::Io)?;
    let mut statx = std::mem::MaybeUninit::<libc::statx>::zeroed();
    let result = unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_BASIC_STATS | STATX_MNT_ID_UNIQUE,
            statx.as_mut_ptr(),
        )
    };
    if result != 0 {
        return Err(EvidenceError::Unsupported);
    }
    let statx = unsafe { statx.assume_init() };
    if statx.stx_mask & STATX_MNT_ID_UNIQUE == 0 || statx.stx_mnt_id == 0 {
        return Err(EvidenceError::Unsupported);
    }
    let boot_uuid = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .and_then(|value| Uuid::parse_str(value.trim()).ok())
        .ok_or(EvidenceError::Unsupported)?;
    Ok(DurableDirectoryIdentity {
        boot_uuid,
        dev: metadata.dev(),
        ino: metadata.ino(),
        statx_mnt_id_unique: statx.stx_mnt_id,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn durable_identity_from_fd(_file: &File) -> EvidenceResult<DurableDirectoryIdentity> {
    Err(EvidenceError::Unsupported)
}

fn directory_identity(file: &File) -> EvidenceResult<DurableDirectoryIdentity> {
    durable_identity_from_fd(file)
}

#[cfg(target_os = "linux")]
pub(crate) fn live_mount_identity_from_fd(file: &File) -> EvidenceResult<LiveMountIdentity> {
    const STATX_MNT_ID: u32 = 0x0000_1000;
    let metadata = file.metadata().map_err(|_| EvidenceError::Io)?;
    let mut statx = std::mem::MaybeUninit::<libc::statx>::zeroed();
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_BASIC_STATS | STATX_MNT_ID,
            statx.as_mut_ptr(),
        )
    } != 0
    {
        return Err(EvidenceError::Unsupported);
    }
    let statx = unsafe { statx.assume_init() };
    if statx.stx_mask & STATX_MNT_ID == 0 || statx.stx_mnt_id == 0 {
        return Err(EvidenceError::Unsupported);
    }
    Ok(LiveMountIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        statx_mnt_id: statx.stx_mnt_id,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn live_mount_identity_from_fd(_file: &File) -> EvidenceResult<LiveMountIdentity> {
    Err(EvidenceError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    struct DirectLinkEnoentGuard;

    #[cfg(target_os = "linux")]
    impl DirectLinkEnoentGuard {
        fn install() -> Self {
            FORCE_DIRECT_LINK_ENOENT.with(|enabled| {
                assert!(
                    !enabled.replace(true),
                    "direct-link failpoint already active"
                );
            });
            Self
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for DirectLinkEnoentGuard {
        fn drop(&mut self) {
            FORCE_DIRECT_LINK_ENOENT.with(|enabled| enabled.set(false));
        }
    }

    #[cfg(target_os = "linux")]
    fn assert_atomic_publication_behavior(directory: &ValidatedDirectory) {
        let leaf = c"record.json";
        let first = atomic_create_json(
            directory,
            leaf,
            &serde_json::json!({"generation": 1}),
            MAX_SPECULATION_JSON_BYTES,
        )
        .unwrap();
        assert_eq!(first.value["generation"], 1);
        assert_eq!(
            atomic_create_json(
                directory,
                leaf,
                &serde_json::json!({"generation": 99}),
                MAX_SPECULATION_JSON_BYTES,
            )
            .unwrap_err(),
            EvidenceError::AlreadyExists
        );
        let second = atomic_replace_json(
            directory,
            leaf,
            first.identity,
            &serde_json::json!({"generation": 2}),
            MAX_SPECULATION_JSON_BYTES,
        )
        .unwrap();
        assert_eq!(second.value["generation"], 2);
        let readback =
            read_json::<serde_json::Value>(directory, leaf, MAX_SPECULATION_JSON_BYTES).unwrap();
        assert_eq!(readback.identity, second.identity);
        assert_eq!(readback.value["generation"], 2);
    }

    #[cfg(target_os = "linux")]
    fn write_private_fixture(path: &Path, bytes: &[u8]) {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o7777, 0o600);
    }

    #[test]
    fn evidence_identity_and_json_cap_are_fixed() {
        let identity = DurableDirectoryIdentity::test_value();
        assert_eq!(identity.dev, 2);
        let serialized = serde_json::to_value(identity).unwrap();
        assert_eq!(serialized["statx_mnt_id_unique"], 4);
        assert!(serialized.get("statx_mnt_id").is_none());
        assert_eq!(MAX_SPECULATION_JSON_BYTES, 32 * 1024);
        assert_eq!(
            EvidenceError::Corrupt.to_string(),
            "speculation_record_corrupt"
        );
    }

    #[test]
    fn leaf_validation_is_strict_and_raw_free() {
        for invalid in [b"".as_slice(), b".", b"..", b"a/b"] {
            let leaf = CString::new(invalid).unwrap();
            assert_eq!(validate_leaf(&leaf), Err(EvidenceError::InvalidLeaf));
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_directory_open_fails_before_mutation() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("must-not-exist");
        assert_eq!(
            open_or_create_private_dir(&target).unwrap_err().to_string(),
            "speculation_evidence_unsupported"
        );
        assert!(!target.exists());
        assert_eq!(
            open_existing_workspace_dir(root.path()).unwrap_err(),
            EvidenceError::Unsupported
        );
        assert_eq!(
            open_existing_delegated_cgroup_root(root.path()).unwrap_err(),
            EvidenceError::Unsupported
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_directory_and_atomic_json_enforce_identity_and_cas() {
        use std::os::unix::fs::DirBuilderExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let directory = open_existing_private_dir(&path).unwrap();
        let leaf = CString::new("record.json").unwrap();
        let first = atomic_create_json(
            &directory,
            &leaf,
            &serde_json::json!({"generation": 1}),
            MAX_SPECULATION_JSON_BYTES,
        )
        .unwrap();
        let second = atomic_replace_json(
            &directory,
            &leaf,
            first.identity,
            &serde_json::json!({"generation": 2}),
            MAX_SPECULATION_JSON_BYTES,
        )
        .unwrap();
        assert_eq!(second.value["generation"], 2);
        assert_eq!(
            atomic_replace_json(
                &directory,
                &leaf,
                first.identity,
                &serde_json::json!({"generation": 3}),
                MAX_SPECULATION_JSON_BYTES
            )
            .unwrap_err()
            .to_string(),
            "speculation_record_stale"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_publication_publishes_create_replace_readback_and_no_replace() {
        use std::os::unix::fs::DirBuilderExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let directory = open_existing_private_dir(&path).unwrap();

        assert_atomic_publication_behavior(&directory);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_fallback_publishes_when_direct_link_returns_enoent() {
        use std::os::unix::fs::DirBuilderExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let directory = open_existing_private_dir(&path).unwrap();
        let _failpoint = DirectLinkEnoentGuard::install();

        assert_atomic_publication_behavior(&directory);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_create_never_publishes_the_unwritten_temporary_inode() {
        use std::os::unix::fs::DirBuilderExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let directory = open_existing_private_dir(&path).unwrap();
        let leaf = c"record.json";
        let bytes = br#"{"generation":1}"#;
        let mut partial = create_unnamed_temp(directory.file.as_raw_fd()).unwrap();
        partial.write_all(br#"{"generation":"#).unwrap();
        partial.sync_all().unwrap();
        drop(partial);
        assert_eq!(
            read_json::<serde_json::Value>(&directory, leaf, MAX_SPECULATION_JSON_BYTES)
                .unwrap_err(),
            EvidenceError::Missing
        );

        let mut temporary = create_unnamed_temp(directory.file.as_raw_fd()).unwrap();
        temporary.write_all(bytes).unwrap();
        temporary.sync_all().unwrap();
        validate_temporary_record_file(&temporary).unwrap();
        assert_eq!(
            read_json::<serde_json::Value>(&directory, leaf, MAX_SPECULATION_JSON_BYTES)
                .unwrap_err(),
            EvidenceError::Missing
        );
        publish_unnamed_no_replace(&temporary, directory.file.as_raw_fd(), leaf).unwrap();
        directory.file.sync_all().unwrap();
        let stored =
            read_json::<serde_json::Value>(&directory, leaf, MAX_SPECULATION_JSON_BYTES).unwrap();
        assert_eq!(stored.value["generation"], 1);
        assert_eq!(
            read_exact_written_json::<serde_json::Value>(
                &directory,
                leaf,
                stored.identity,
                br#"{"generation":2}"#,
                MAX_SPECULATION_JSON_BYTES,
            )
            .unwrap_err(),
            EvidenceError::Corrupt
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn concurrent_compare_and_swap_has_exactly_one_winner() {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::{Arc, Barrier};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let first_directory = open_existing_private_dir(&path).unwrap();
        let leaf = c"record.json";
        let initial = atomic_create_json(
            &first_directory,
            leaf,
            &serde_json::json!({"generation": 1}),
            MAX_SPECULATION_JSON_BYTES,
        )
        .unwrap();
        let second_directory = open_existing_private_dir(&path).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let spawn_writer =
            |directory: ValidatedDirectory, generation: u64, barrier: Arc<Barrier>| {
                std::thread::spawn(move || {
                    barrier.wait();
                    atomic_replace_json(
                        &directory,
                        leaf,
                        initial.identity,
                        &serde_json::json!({"generation": generation}),
                        MAX_SPECULATION_JSON_BYTES,
                    )
                })
            };
        let left = spawn_writer(first_directory, 2, Arc::clone(&barrier));
        let right = spawn_writer(second_directory, 3, Arc::clone(&barrier));
        barrier.wait();
        let results = [left.join().unwrap(), right.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(EvidenceError::Stale)))
                .count(),
            1
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nested_private_trees_revalidate_and_enumeration_stays_on_retained_fd() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let directory = open_existing_private_dir(&path).unwrap();
        let child = path.join("src");
        std::fs::create_dir(&child).unwrap();
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
        directory.revalidate().unwrap();

        std::fs::write(path.join("retained"), b"old").unwrap();
        let moved = root.path().join("moved");
        std::fs::rename(&path, &moved).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        std::fs::write(path.join("replacement"), b"new").unwrap();
        let names = enumerate_leaf_names_from_fd(&directory.file, directory.identity()).unwrap();
        assert!(names.iter().any(|name| name.to_bytes() == b"retained"));
        assert!(!names.iter().any(|name| name.to_bytes() == b"replacement"));
        assert_eq!(directory.list_leaf_names(), Err(EvidenceError::Stale));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn validated_transaction_temp_is_reaped_without_poisoning_enumeration() {
        use std::os::unix::fs::DirBuilderExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let directory = open_existing_private_dir(&path).unwrap();
        let temp = transaction_temp_leaf();
        let mut file = create_unnamed_temp(directory.file.as_raw_fd()).unwrap();
        file.write_all(br#"{"complete":true}"#).unwrap();
        file.sync_all().unwrap();
        publish_unnamed_no_replace(&file, directory.file.as_raw_fd(), &temp).unwrap();
        assert!(directory.list_leaf_names().unwrap().is_empty());
        assert_eq!(
            read_json::<serde_json::Value>(&directory, &temp, MAX_SPECULATION_JSON_BYTES)
                .unwrap_err(),
            EvidenceError::Missing
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn invalid_and_oversized_transaction_temps_remain_corruption_evidence() {
        use std::os::unix::fs::DirBuilderExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let directory = open_existing_private_dir(&path).unwrap();

        let corrupt_temp = transaction_temp_leaf();
        let corrupt_path = path.join(std::ffi::OsStr::from_bytes(corrupt_temp.to_bytes()));
        write_private_fixture(&corrupt_path, br#"{"complete":"#);
        assert_eq!(
            validate_temp_leaf_for_recovery(directory.file.as_raw_fd(), &corrupt_temp),
            Err(EvidenceError::Corrupt)
        );

        let oversized_temp = transaction_temp_leaf();
        let oversized_path = path.join(std::ffi::OsStr::from_bytes(oversized_temp.to_bytes()));
        let mut oversized_json = vec![b' '; MAX_SPECULATION_JSON_BYTES];
        oversized_json.extend_from_slice(b"null");
        write_private_fixture(&oversized_path, &oversized_json);
        assert_eq!(
            validate_temp_leaf_for_recovery(directory.file.as_raw_fd(), &oversized_temp),
            Err(EvidenceError::TooLarge)
        );

        let leaves = directory.list_leaf_names().unwrap();
        assert!(leaves.iter().any(|leaf| leaf == &corrupt_temp));
        assert!(leaves.iter().any(|leaf| leaf == &oversized_temp));
        assert!(corrupt_path.exists());
        assert!(oversized_path.exists());
    }
}
