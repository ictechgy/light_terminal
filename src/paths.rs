use anyhow::{Context, Result, bail};
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const APP_DIR_NAME: &str = "light-terminal";
const ACTIVE_SOCKET_MARKER: &str = "active-socket";

pub fn runtime_dir() -> Result<PathBuf> {
    if let Ok(dir) = env::var("LTERM_RUNTIME_DIR") {
        let path = PathBuf::from(dir);
        require_absolute_env_path("LTERM_RUNTIME_DIR", &path)?;
        ensure_user_private_dir(&path)?;
        return Ok(path);
    }

    if let Some(base) = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        require_absolute_env_path("XDG_RUNTIME_DIR", &base)?;
        validate_existing_private_dir(&base)?;
        let path = base.join(APP_DIR_NAME);
        ensure_private_dir(&path)?;
        return Ok(path);
    }

    let path = env::temp_dir().join(format!("{}-{}", APP_DIR_NAME, current_euid()));
    ensure_private_dir(&path)?;
    Ok(path)
}

pub fn data_dir() -> Result<PathBuf> {
    if let Ok(dir) = env::var("LTERM_DATA_DIR") {
        let path = PathBuf::from(dir);
        require_absolute_env_path("LTERM_DATA_DIR", &path)?;
        ensure_user_private_dir(&path)?;
        return Ok(path);
    }

    let path = if let Some(home) = env::var_os("HOME") {
        PathBuf::from(home).join(".local/share").join(APP_DIR_NAME)
    } else {
        runtime_dir()?.join("data")
    };
    ensure_private_dir(&path)?;
    Ok(path)
}

/// Private root of the Linux managed-process registry.
///
/// Creation and exact metadata validation belong to `launch_registry`: unlike
/// ordinary application directories, registry genesis must be an all-or-
/// nothing, no-replace transaction.
#[cfg(target_os = "linux")]
pub(crate) fn process_registry_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("speculation").join("process-registry-v1"))
}

pub fn socket_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("LTERM_SOCKET") {
        let path = PathBuf::from(path);
        require_absolute_env_path("LTERM_SOCKET", &path)?;
        validate_socket_parent(&path)?;
        return Ok(path);
    }
    if env::var_os("LTERM_RUNTIME_DIR").is_some() || env::var_os("XDG_RUNTIME_DIR").is_some() {
        let path = runtime_dir()?.join("lterm.sock");
        validate_socket_leaf(&path)?;
        return Ok(path);
    }
    if let Some(path) = recorded_default_socket_path()? {
        return Ok(path);
    }
    let path = runtime_dir()?.join("lterm.sock");
    validate_socket_leaf(&path)?;
    Ok(path)
}

/// Socket-shaped marker used only for `$TMUX` compatibility metadata.
///
/// lterm children still receive `LTERM_SOCKET` as the live daemon transport
/// used by the shim. `$TMUX` intentionally points at this non-listening sibling
/// path so an accidentally resolved real `tmux` binary fails quickly instead of
/// blocking while trying to speak tmux protocol to the lterm daemon socket.
pub fn tmux_compat_socket_path() -> Result<PathBuf> {
    let socket = socket_path()?;
    let parent = socket
        .parent()
        .context("LTERM_SOCKET must include a parent directory")?;
    let basename = socket
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "lterm.sock".into());
    Ok(parent.join(format!(".{basename}.tmux-compat")))
}

pub fn log_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("daemon.log"))
}

/// Owner-private storage root for the bounded session lifecycle journal.
///
/// The journal implementation opens this directory with `O_DIRECTORY`,
/// `O_NOFOLLOW`, and `O_CLOEXEC`, then performs every leaf operation relative
/// to that descriptor. Keeping path construction here ensures all daemon
/// instances use the same already-private data root without exposing journal
/// paths to protocol or client code.
pub(crate) fn session_lifecycle_dir() -> Result<PathBuf> {
    let path = data_dir()?.join("session-lifecycle-v1");
    ensure_private_dir(&path)?;
    Ok(path)
}

pub(crate) fn record_default_socket_path(socket: &Path) -> Result<()> {
    if env::var_os("LTERM_SOCKET").is_some()
        || env::var_os("LTERM_RUNTIME_DIR").is_some()
        || env::var_os("XDG_RUNTIME_DIR").is_some()
    {
        return Ok(());
    }
    require_absolute_env_path("active socket path", socket)?;
    validate_socket_parent(socket)?;

    let marker = active_socket_marker_path()?;
    let tmp = marker.with_file_name(format!(
        ".{ACTIVE_SOCKET_MARKER}.{}.tmp",
        std::process::id()
    ));
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("open temporary active socket marker {}", tmp.display()))?;
    writeln!(file, "{}", socket.display())
        .with_context(|| format!("write active socket marker {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("sync active socket marker {}", tmp.display()))?;
    fs::rename(&tmp, &marker)
        .with_context(|| format!("replace active socket marker {}", marker.display()))?;
    Ok(())
}

pub fn shim_dir() -> Result<PathBuf> {
    let path = data_dir()?.join("shims");
    ensure_private_dir(&path)?;
    Ok(path)
}

pub fn store_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("tmux-compat-store.json"))
}

pub fn store_lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("tmux-compat-store.lock"))
}

pub fn buffer_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("tmux-buffer.txt"))
}

pub fn reconnect_state_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("reconnect-state.json"))
}

fn active_socket_marker_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(ACTIVE_SOCKET_MARKER))
}

fn default_active_socket_marker_path() -> Result<Option<PathBuf>> {
    if env::var_os("LTERM_DATA_DIR").is_some() {
        return Ok(Some(active_socket_marker_path()?));
    }
    let Some(home) = env::var_os("HOME") else {
        return Ok(None);
    };
    let path = PathBuf::from(home).join(".local/share").join(APP_DIR_NAME);
    ensure_private_dir(&path)?;
    Ok(Some(path.join(ACTIVE_SOCKET_MARKER)))
}

fn recorded_default_socket_path() -> Result<Option<PathBuf>> {
    let Some(marker) = default_active_socket_marker_path()? else {
        return Ok(None);
    };
    let text = match fs::read_to_string(&marker) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", marker.display())),
    };
    let raw = text.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Ok(None);
    }
    if validate_socket_parent(&path).is_err() {
        return Ok(None);
    }
    Ok(Some(path))
}

pub(crate) fn ensure_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => validate_private_dir_metadata(path, &meta)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .with_context(|| format!("create private directory {}", path.display()))?;
            let meta =
                fs::symlink_metadata(path).with_context(|| format!("lstat {}", path.display()))?;
            validate_private_dir_metadata(path, &meta)?;
        }
        Err(err) => return Err(err).with_context(|| format!("lstat {}", path.display())),
    }
    Ok(())
}

fn ensure_user_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => validate_private_dir_metadata_without_chmod(path, &meta),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .with_context(|| format!("create private directory {}", path.display()))?;
            let meta =
                fs::symlink_metadata(path).with_context(|| format!("lstat {}", path.display()))?;
            validate_private_dir_metadata_without_chmod(path, &meta)
        }
        Err(err) => Err(err).with_context(|| format!("lstat {}", path.display())),
    }
}

fn validate_existing_private_dir(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("lstat {}", path.display()))?;
    validate_private_dir_metadata_without_chmod(path, &meta)
}

fn validate_private_dir_metadata(path: &Path, meta: &fs::Metadata) -> Result<()> {
    validate_private_dir_base_metadata(path, meta)?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        let dir = open_private_dir_no_follow(path)?;
        let handle_meta = dir
            .metadata()
            .with_context(|| format!("fstat {}", path.display()))?;
        validate_private_dir_base_metadata(path, &handle_meta)?;

        let handle_mode = handle_meta.permissions().mode() & 0o777;
        if handle_mode & 0o077 != 0 {
            let mut perms = handle_meta.permissions();
            perms.set_mode(handle_mode & !0o077);
            dir.set_permissions(perms)
                .with_context(|| format!("tighten permissions on {}", path.display()))?;
        }

        let tightened = dir
            .metadata()
            .with_context(|| format!("fstat {}", path.display()))?;
        validate_private_dir_base_metadata(path, &tightened)?;
        let tightened_mode = tightened.permissions().mode() & 0o777;
        if tightened_mode & 0o077 != 0 {
            bail!(
                "{} must be private (mode 0700 or stricter), found {:03o}",
                path.display(),
                tightened_mode
            );
        }

        // Re-check the path after descriptor-based chmod so callers do not proceed if
        // the pathname was swapped while permissions were being tightened.
        let latest =
            fs::symlink_metadata(path).with_context(|| format!("lstat {}", path.display()))?;
        validate_private_dir_base_metadata(path, &latest)?;
        let latest_mode = latest.permissions().mode() & 0o777;
        if latest_mode & 0o077 != 0 {
            bail!(
                "{} changed while tightening permissions (mode {:03o})",
                path.display(),
                latest_mode
            );
        }
    }
    Ok(())
}

fn open_private_dir_no_follow(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC);
    options
        .open(path)
        .with_context(|| format!("open private directory {}", path.display()))
}

fn validate_private_dir_metadata_without_chmod(path: &Path, meta: &fs::Metadata) -> Result<()> {
    validate_private_dir_base_metadata(path, meta)?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} must be private (mode 0700 or stricter), found {:03o}",
            path.display(),
            mode
        );
    }
    Ok(())
}

fn validate_private_dir_base_metadata(path: &Path, meta: &fs::Metadata) -> Result<()> {
    if meta.file_type().is_symlink() {
        bail!("{} must not be a symlink", path.display());
    }
    if !meta.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    let uid = current_euid();
    if meta.uid() != uid {
        bail!(
            "{} is owned by uid {}, expected uid {}",
            path.display(),
            meta.uid(),
            uid
        );
    }
    Ok(())
}

fn validate_socket_parent(socket: &Path) -> Result<()> {
    if socket.as_os_str().is_empty() {
        bail!("LTERM_SOCKET cannot be empty");
    }
    validate_socket_leaf(socket)?;
    let parent = socket
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("LTERM_SOCKET must include a parent directory")?;
    ensure_user_private_dir(parent)
}

fn validate_socket_leaf(socket: &Path) -> Result<()> {
    match fs::symlink_metadata(socket) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!("{} must not be a symlink", socket.display());
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("lstat {}", socket.display())),
    }
    Ok(())
}

fn require_absolute_env_path(name: &str, path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("{name} must be an absolute path: {}", path.display());
    }
    Ok(())
}

pub(crate) fn current_euid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::{
        APP_DIR_NAME, data_dir, ensure_private_dir, record_default_socket_path, runtime_dir,
        socket_path, tmux_compat_socket_path, validate_socket_parent,
    };
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use std::process::Command;

    const PATH_ENV_NAMES: [&str; 6] = [
        "LTERM_RUNTIME_DIR",
        "LTERM_DATA_DIR",
        "LTERM_SOCKET",
        "XDG_RUNTIME_DIR",
        "TMPDIR",
        "HOME",
    ];
    const PATH_ENV_SELF_REEXEC: &str = "LTERM_TEST_PATH_ENV_SELF_REEXEC";

    struct EnvGuard {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        fn capture(names: &[&'static str]) -> Self {
            let saved = names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect();
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: paths tests hold crate::TEST_ENV_LOCK while mutating process env.
            unsafe {
                for (name, value) in &self.saved {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    fn reset_path_env() -> EnvGuard {
        let guard = EnvGuard::capture(&PATH_ENV_NAMES);
        // SAFETY: caller holds crate::TEST_ENV_LOCK and EnvGuard restores values.
        unsafe {
            for name in PATH_ENV_NAMES {
                std::env::remove_var(name);
            }
        }
        guard
    }

    fn path_env_snapshot() -> Vec<(&'static str, Option<OsString>)> {
        PATH_ENV_NAMES
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect()
    }

    fn in_isolated_path_env(body: impl FnOnce()) {
        let current_thread = std::thread::current();
        let test_name = current_thread
            .name()
            .expect("path test thread must have an exact libtest name");
        let _lock = crate::TEST_ENV_LOCK.lock().expect("env lock");
        match std::env::var_os(PATH_ENV_SELF_REEXEC) {
            Some(marker) => {
                assert_eq!(
                    marker,
                    OsString::from(test_name),
                    "path test self-reexec marker must match the exact libtest name"
                );
                let _env = reset_path_env();
                body();
            }
            None => {
                let before = path_env_snapshot();
                let output = Command::new(std::env::current_exe().expect("current test binary"))
                    .args(["--exact", test_name, "--nocapture"])
                    .env(PATH_ENV_SELF_REEXEC, test_name)
                    .output();
                let after = path_env_snapshot();
                assert_eq!(after, before, "self-reexec parent path environment changed");
                let output = output.expect("spawn exact path-test self-reexec child");
                assert!(
                    output.status.success(),
                    "exact path-test self-reexec child failed: test={test_name:?} status={} stdout={} stderr={}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }

    #[test]
    fn runtime_dir_rejects_relative_lterm_runtime_dir() {
        in_isolated_path_env(|| {
            // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
            unsafe {
                std::env::set_var("LTERM_RUNTIME_DIR", "relative-runtime");
            }

            let err = runtime_dir().expect_err("relative runtime dir must fail");
            assert!(
                err.to_string()
                    .contains("LTERM_RUNTIME_DIR must be an absolute path"),
                "unexpected error: {err:#}"
            );
        });
    }

    #[test]
    fn data_dir_rejects_relative_lterm_data_dir() {
        in_isolated_path_env(|| {
            // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
            unsafe {
                std::env::set_var("LTERM_DATA_DIR", "relative-data");
            }

            let err = data_dir().expect_err("relative data dir must fail");
            assert!(
                err.to_string()
                    .contains("LTERM_DATA_DIR must be an absolute path"),
                "unexpected error: {err:#}"
            );
        });
    }

    #[test]
    fn data_dir_without_home_falls_back_to_tmp_runtime_data_dir() {
        in_isolated_path_env(|| {
            let tmp = tempfile::tempdir().expect("temp dir");
            // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
            unsafe {
                std::env::remove_var("HOME");
                std::env::set_var("TMPDIR", tmp.path());
            }

            let path = data_dir().expect("HOME-less default data dir should use runtime fallback");
            assert!(
                path.starts_with(tmp.path()),
                "HOME-less data dir should stay in the sandboxed temp runtime, got {path:?}"
            );
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some("data")
            );
            let mode = fs::symlink_metadata(&path)
                .expect("created fallback data dir")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode & 0o077, 0, "fallback data dir must be private");
        });
    }

    #[test]
    fn runtime_dir_uses_xdg_runtime_dir_child_and_preserves_private_base() {
        in_isolated_path_env(|| {
            let xdg = tempfile::tempdir().expect("temp xdg runtime dir");
            fs::set_permissions(xdg.path(), fs::Permissions::from_mode(0o700))
                .expect("private xdg dir");
            // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
            unsafe {
                std::env::set_var("XDG_RUNTIME_DIR", xdg.path());
            }

            let path = runtime_dir().expect("runtime dir under xdg");
            assert_eq!(path, xdg.path().join(APP_DIR_NAME));
            let mode = fs::symlink_metadata(&path)
                .expect("created runtime child")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode & 0o077, 0, "runtime child must be private");
        });
    }

    #[test]
    fn socket_path_rejects_relative_lterm_socket() {
        in_isolated_path_env(|| {
            // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
            unsafe {
                std::env::set_var("LTERM_SOCKET", "relative.sock");
            }

            let err = socket_path().expect_err("relative socket override must fail");
            assert!(
                err.to_string()
                    .contains("LTERM_SOCKET must be an absolute path"),
                "unexpected error: {err:#}"
            );
        });
    }

    #[test]
    fn validate_socket_parent_rejects_empty_and_parentless_paths() {
        let err = validate_socket_parent(Path::new("")).expect_err("empty socket path must fail");
        assert!(
            err.to_string().contains("LTERM_SOCKET cannot be empty"),
            "unexpected empty-path error: {err:#}"
        );

        let err = validate_socket_parent(Path::new("lterm.sock"))
            .expect_err("parentless socket path must fail");
        assert!(
            err.to_string()
                .contains("LTERM_SOCKET must include a parent directory"),
            "unexpected parentless-path error: {err:#}"
        );
    }

    #[test]
    fn tmux_compat_socket_is_private_sibling_not_live_socket() {
        in_isolated_path_env(|| {
            let dir = tempfile::tempdir().expect("temp socket dir");
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
                .expect("private socket dir");
            let socket = dir.path().join("lterm.sock");
            // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
            unsafe {
                std::env::set_var("LTERM_SOCKET", &socket);
            }

            let compat = tmux_compat_socket_path().expect("compat socket path");
            assert_ne!(
                compat, socket,
                "compat socket must not be the live daemon socket"
            );
            assert_eq!(compat.parent(), socket.parent());
            assert_eq!(
                compat.file_name().and_then(|name| name.to_str()),
                Some(".lterm.sock.tmux-compat")
            );
        });
    }

    #[test]
    fn socket_path_rejects_symlink_leaf() {
        in_isolated_path_env(|| {
            let dir = tempfile::tempdir().expect("temp socket dir");
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
                .expect("private socket dir");
            let target = dir.path().join("target.sock");
            fs::write(&target, b"not a socket").expect("target file");
            let link = dir.path().join("lterm.sock");
            symlink(&target, &link).expect("socket leaf symlink");
            // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
            unsafe {
                std::env::set_var("LTERM_SOCKET", &link);
            }

            let err = socket_path().expect_err("symlink socket leaf must fail");
            assert!(
                err.to_string().contains("must not be a symlink"),
                "unexpected error: {err:#}"
            );
        });
    }

    #[test]
    fn socket_path_reuses_recorded_default_socket_when_tmpdir_changes() {
        in_isolated_path_env(|| {
            let data = tempfile::tempdir().expect("temp data dir");
            let tmp_a = tempfile::tempdir().expect("first temp dir");
            let tmp_b = tempfile::tempdir().expect("second temp dir");
            fs::set_permissions(data.path(), fs::Permissions::from_mode(0o700))
                .expect("private data dir");
            // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
            unsafe {
                std::env::set_var("LTERM_DATA_DIR", data.path());
                std::env::set_var("TMPDIR", tmp_a.path());
            }

            let first = socket_path().expect("first default socket path");
            assert!(
                first.starts_with(tmp_a.path()),
                "initial fallback should use the current temp dir before a marker exists: {first:?}"
            );
            record_default_socket_path(&first).expect("record active default socket");

            // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
            unsafe {
                std::env::set_var("TMPDIR", tmp_b.path());
            }
            assert_eq!(
                socket_path().expect("socket path from marker"),
                first,
                "default clients should rejoin the active daemon even if TMPDIR changes"
            );
        });
    }

    #[test]
    fn socket_path_without_home_skips_default_marker_and_uses_tmp_runtime() {
        in_isolated_path_env(|| {
            let tmp = tempfile::tempdir().expect("temp dir");
            // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
            unsafe {
                std::env::remove_var("HOME");
                std::env::set_var("TMPDIR", tmp.path());
            }

            let path = socket_path().expect("HOME-less default socket path should still work");
            assert!(
                path.starts_with(tmp.path()),
                "HOME-less default should fall back to temp runtime, got {path:?}"
            );
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some("lterm.sock")
            );
        });
    }

    #[test]
    fn explicit_data_dir_errors_remain_strict_for_default_marker_lookup() {
        in_isolated_path_env(|| {
            let tmp = tempfile::tempdir().expect("temp dir");
            // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
            unsafe {
                std::env::remove_var("HOME");
                std::env::set_var("TMPDIR", tmp.path());
                std::env::set_var("LTERM_DATA_DIR", "relative-data");
            }

            let err = socket_path().expect_err("explicit invalid data dir must not be ignored");
            assert!(
                err.to_string()
                    .contains("LTERM_DATA_DIR must be an absolute path"),
                "unexpected error: {err:#}"
            );
        });
    }

    #[test]
    fn explicit_runtime_dir_ignores_recorded_default_socket() {
        in_isolated_path_env(|| {
            let data = tempfile::tempdir().expect("temp data dir");
            let tmp = tempfile::tempdir().expect("temp dir");
            let explicit = tempfile::tempdir().expect("explicit runtime dir");
            fs::set_permissions(data.path(), fs::Permissions::from_mode(0o700))
                .expect("private data dir");
            fs::set_permissions(explicit.path(), fs::Permissions::from_mode(0o700))
                .expect("private explicit runtime dir");
            // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
            unsafe {
                std::env::set_var("LTERM_DATA_DIR", data.path());
                std::env::set_var("TMPDIR", tmp.path());
            }
            let recorded = socket_path().expect("recorded socket source");
            record_default_socket_path(&recorded).expect("record active default socket");

            // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
            unsafe {
                std::env::set_var("LTERM_RUNTIME_DIR", explicit.path());
            }
            assert_eq!(
                socket_path().expect("explicit runtime socket path"),
                explicit.path().join("lterm.sock"),
                "explicit runtime overrides must not be hijacked by the default marker"
            );
        });
    }

    #[test]
    fn ensure_private_dir_tightens_world_accessible_existing_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))
            .expect("world-readable dir");

        ensure_private_dir(dir.path()).expect("tighten existing private dir");
        let mode = fs::symlink_metadata(dir.path())
            .expect("tightened dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode & 0o077, 0, "group/world bits must be cleared");
    }

    #[test]
    fn ensure_private_dir_refuses_symlink_without_chmod_target() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("target");
        fs::create_dir(&target).expect("target dir");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
            .expect("world-readable target dir");
        let link = dir.path().join("link");
        symlink(&target, &link).expect("symlink private dir candidate");

        let err = ensure_private_dir(&link).expect_err("symlink private dir must fail");
        assert!(
            err.to_string().contains("must not be a symlink"),
            "unexpected symlink error: {err:#}"
        );
        let target_mode = fs::symlink_metadata(&target)
            .expect("target metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            target_mode, 0o755,
            "symlink target permissions must not be tightened through the link"
        );
    }

    #[test]
    fn user_private_socket_parent_refuses_world_accessible_override_parent() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))
            .expect("world-readable dir");

        let err = validate_socket_parent(&dir.path().join("lterm.sock"))
            .expect_err("user override parent must already be private");
        assert!(
            err.to_string().contains("must be private"),
            "unexpected error: {err:#}"
        );
    }
}
