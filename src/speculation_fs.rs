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
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

pub const MAX_SPECULATION_JSON_BYTES: usize = 32 * 1024;
const MAX_DIRECTORY_PATH_BYTES: usize = 4096;
const MAX_LEAF_BYTES: usize = 255;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

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
pub struct DirectoryIdentity {
    pub boot_uuid: Uuid,
    pub dev: u64,
    pub ino: u64,
    pub statx_mnt_id: u64,
}

#[cfg(test)]
impl DirectoryIdentity {
    pub fn test_value() -> Self {
        Self {
            boot_uuid: Uuid::nil(),
            dev: 2,
            ino: 3,
            statx_mnt_id: 4,
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
    identity: DirectoryIdentity,
    canonical_locator: PathBuf,
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
    pub fn identity(&self) -> DirectoryIdentity {
        self.identity
    }

    pub fn revalidate(&self) -> EvidenceResult<()> {
        let current = directory_identity(&self.file)?;
        if current != self.identity {
            return Err(EvidenceError::Stale);
        }
        validate_private_directory_metadata(&self.file)?;
        self.reopen_and_verify()
    }

    pub fn reopen_and_verify(&self) -> EvidenceResult<()> {
        let reopened = open_existing_private_dir(&self.canonical_locator)?;
        if reopened.identity != self.identity {
            return Err(EvidenceError::Stale);
        }
        Ok(())
    }

    pub(crate) fn canonical_locator_bytes(&self) -> &[u8] {
        self.canonical_locator.as_os_str().as_bytes()
    }

    pub(crate) fn list_leaf_names(&self) -> EvidenceResult<Vec<CString>> {
        self.revalidate()?;
        let entries = std::fs::read_dir(&self.canonical_locator).map_err(|_| EvidenceError::Io)?;
        let mut names = Vec::new();
        for entry in entries {
            let name = entry.map_err(|_| EvidenceError::Io)?.file_name();
            names.push(CString::new(name.as_bytes()).map_err(|_| EvidenceError::InvalidLeaf)?);
        }
        Ok(names)
    }
}

pub fn open_existing_private_dir(path: &Path) -> EvidenceResult<ValidatedDirectory> {
    open_private_dir(path, false)
}

pub fn open_or_create_private_dir(path: &Path) -> EvidenceResult<ValidatedDirectory> {
    open_private_dir(path, true)
}

#[cfg(target_os = "linux")]
fn open_private_dir(path: &Path, create: bool) -> EvidenceResult<ValidatedDirectory> {
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
    validate_private_directory_metadata(&canonical)?;
    let identity = directory_identity(&canonical)?;
    Ok(ValidatedDirectory {
        file: canonical,
        identity,
        canonical_locator: canonical_path,
    })
}

#[cfg(not(target_os = "linux"))]
fn open_private_dir(_path: &Path, _create: bool) -> EvidenceResult<ValidatedDirectory> {
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
    directory.revalidate()?;
    validate_leaf(leaf)?;
    let bytes = serialize_bounded(value, cap)?;
    let mut file = openat_file(
        directory.file.as_raw_fd(),
        leaf,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0o600,
    )?;
    write_and_sync(&mut file, &bytes)?;
    validate_record_file(&file)?;
    directory.file.sync_all().map_err(|_| EvidenceError::Io)?;
    read_json(directory, leaf, cap)
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
    directory.revalidate()?;
    validate_leaf(leaf)?;
    let current = openat_file(
        directory.file.as_raw_fd(),
        leaf,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    if validate_record_file(&current)? != expected {
        return Err(EvidenceError::Stale);
    }

    let bytes = serialize_bounded(value, cap)?;
    let temp = temp_leaf()?;
    let result = (|| {
        let mut file = openat_file(
            directory.file.as_raw_fd(),
            &temp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )?;
        write_and_sync(&mut file, &bytes)?;
        validate_record_file(&file)?;
        renameat(directory.file.as_raw_fd(), &temp, leaf)?;
        directory.file.sync_all().map_err(|_| EvidenceError::Io)?;
        read_json(directory, leaf, cap)
    })();
    if result.is_err() {
        unlinkat(directory.file.as_raw_fd(), &temp);
    }
    result
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
        || metadata.nlink() != 1
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
        || metadata.nlink() != 2
    {
        return Err(EvidenceError::InvalidDirectory);
    }
    Ok(())
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

fn temp_leaf() -> EvidenceResult<CString> {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    CString::new(format!(
        ".lterm-speculation-{}-{id}.tmp",
        std::process::id()
    ))
    .map_err(|_| EvidenceError::InvalidLeaf)
}

fn openat_file(dirfd: RawFd, leaf: &CStr, flags: i32, mode: u32) -> EvidenceResult<File> {
    let fd = unsafe { libc::openat(dirfd, leaf.as_ptr(), flags, mode as libc::c_uint) };
    if fd < 0 {
        return Err(map_io_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn renameat(dirfd: RawFd, from: &CStr, to: &CStr) -> EvidenceResult<()> {
    if unsafe { libc::renameat(dirfd, from.as_ptr(), dirfd, to.as_ptr()) } != 0 {
        return Err(map_io_error());
    }
    Ok(())
}

fn unlinkat(dirfd: RawFd, leaf: &CStr) {
    unsafe {
        libc::unlinkat(dirfd, leaf.as_ptr(), 0);
    }
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
fn create_and_open_components(path: &Path) -> EvidenceResult<File> {
    open_components(path, true)
}

#[cfg(target_os = "linux")]
fn open_components(path: &Path, create: bool) -> EvidenceResult<File> {
    let root_fd = unsafe {
        libc::open(
            b"/\0".as_ptr().cast(),
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
fn directory_identity(file: &File) -> EvidenceResult<DirectoryIdentity> {
    const STATX_MNT_ID_UNIQUE: u32 = 0x0000_4000;
    let metadata = file.metadata().map_err(|_| EvidenceError::Io)?;
    let mut statx = std::mem::MaybeUninit::<libc::statx>::zeroed();
    let result = unsafe {
        libc::statx(
            file.as_raw_fd(),
            b"\0".as_ptr().cast(),
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
    Ok(DirectoryIdentity {
        boot_uuid,
        dev: metadata.dev(),
        ino: metadata.ino(),
        statx_mnt_id: statx.stx_mnt_id,
    })
}

#[cfg(not(target_os = "linux"))]
fn directory_identity(_file: &File) -> EvidenceResult<DirectoryIdentity> {
    Err(EvidenceError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_identity_and_json_cap_are_fixed() {
        let identity = DirectoryIdentity::test_value();
        assert_eq!(identity.dev, 2);
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
}
