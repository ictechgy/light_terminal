use anyhow::{Context, Result, bail};
use std::env;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const APP_DIR_NAME: &str = "light-terminal";

pub fn runtime_dir() -> Result<PathBuf> {
    if let Ok(dir) = env::var("LTERM_RUNTIME_DIR") {
        let path = PathBuf::from(dir);
        ensure_private_dir(&path)?;
        return Ok(path);
    }

    if let Some(base) = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        ensure_private_dir(&base)?;
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
        ensure_private_dir(&path)?;
        return Ok(path);
    }

    let home = env::var_os("HOME").context("HOME is not set")?;
    let path = PathBuf::from(home).join(".local/share").join(APP_DIR_NAME);
    ensure_private_dir(&path)?;
    Ok(path)
}

pub fn socket_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("LTERM_SOCKET") {
        return Ok(PathBuf::from(path));
    }
    Ok(runtime_dir()?.join("lterm.sock"))
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

fn ensure_private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .with_context(|| format!("create private directory {}", path.display()))?;
    }

    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
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
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        let mut perms = meta.permissions();
        perms.set_mode(mode & !0o077);
        fs::set_permissions(path, perms)
            .with_context(|| format!("tighten permissions on {}", path.display()))?;
    }
    Ok(())
}

fn current_euid() -> u32 {
    unsafe { geteuid() }
}

unsafe extern "C" {
    fn geteuid() -> u32;
}
