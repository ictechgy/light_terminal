use anyhow::{Context, Result, bail};
use std::env;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const APP_DIR_NAME: &str = "light-terminal";

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

    let home = env::var_os("HOME").context("HOME is not set")?;
    let path = PathBuf::from(home).join(".local/share").join(APP_DIR_NAME);
    ensure_private_dir(&path)?;
    Ok(path)
}

pub fn socket_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("LTERM_SOCKET") {
        let path = PathBuf::from(path);
        require_absolute_env_path("LTERM_SOCKET", &path)?;
        validate_socket_parent(&path)?;
        return Ok(path);
    }
    let path = runtime_dir()?.join("lterm.sock");
    validate_socket_leaf(&path)?;
    Ok(path)
}

pub fn log_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("daemon.log"))
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
        let mut perms = meta.permissions();
        perms.set_mode(mode & !0o077);
        fs::set_permissions(path, perms)
            .with_context(|| format!("tighten permissions on {}", path.display()))?;
        let tightened =
            fs::symlink_metadata(path).with_context(|| format!("lstat {}", path.display()))?;
        if tightened.file_type().is_symlink() {
            bail!(
                "{} became a symlink while tightening permissions",
                path.display()
            );
        }
    }
    Ok(())
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
