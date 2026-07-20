#![cfg(target_os = "linux")]

use std::env;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::mem::{self, MaybeUninit};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const REQUIRE_REAL_GATE_ENV: &str = "LTERM_REQUIRE_REAL_BWRAP";
const CGROUP_ROOT_ENV: &str = "LTERM_G003_CGROUP_ROOT";
const INTERNAL_MANAGED_LAUNCH_ARG: &str = "__lterm-internal-managed-launch-test-v1";
const STATX_MNT_ID_UNIQUE: u32 = 0x0000_4000;
const CGROUP2_SUPER_MAGIC: libc::c_long = 0x6367_7270;

#[test]
#[ignore = "required non-skipping Ubuntu Gate P; run only with the explicit real-gate environment"]
fn g003_real_bwrap_cgroup_feasibility() {
    assert_eq!(
        env::var_os(REQUIRE_REAL_GATE_ENV).as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "the required real Gate P environment flag is absent"
    );

    let cgroup_root = PathBuf::from(
        env::var_os(CGROUP_ROOT_ENV).expect("the required delegated cgroup root is absent"),
    );
    let mut completed_real_cases = 0usize;

    prove_unique_mount_id(&cgroup_root);
    completed_real_cases += 1;

    prove_seqpacket_peer_credentials();
    completed_real_cases += 1;

    prove_delegated_domain_cgroup(&cgroup_root);
    completed_real_cases += 1;

    prove_pinned_bwrap_namespaces();
    completed_real_cases += 1;

    assert_eq!(
        completed_real_cases, 4,
        "the required Gate P did not execute every real case"
    );
}

fn prove_unique_mount_id(path: &Path) {
    let path = CString::new(path.as_os_str().as_bytes()).expect("cgroup root contains NUL");
    let mut observed = MaybeUninit::<libc::statx>::zeroed();
    let result = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_BASIC_STATS | STATX_MNT_ID_UNIQUE,
            observed.as_mut_ptr(),
        )
    };
    assert_eq!(
        result,
        0,
        "statx STATX_MNT_ID_UNIQUE failed: {}",
        std::io::Error::last_os_error()
    );
    let observed = unsafe { observed.assume_init() };
    assert_ne!(
        observed.stx_mask & STATX_MNT_ID_UNIQUE,
        0,
        "statx omitted the requested STATX_MNT_ID_UNIQUE field"
    );
    assert_ne!(
        observed.stx_mnt_id, 0,
        "statx returned a zero unique mount ID"
    );
}

fn prove_seqpacket_peer_credentials() {
    let temp = tempfile::tempdir().expect("create private seqpacket fixture");
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
        .expect("secure seqpacket fixture");
    let socket_path = temp.path().join("peer.sock");
    let (address, address_length) = unix_socket_address(&socket_path);

    let listener = owned_socket();
    let bind_result = unsafe {
        libc::bind(
            listener.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            address_length,
        )
    };
    assert_eq!(
        bind_result,
        0,
        "bind SOCK_SEQPACKET listener: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(
        unsafe { libc::listen(listener.as_raw_fd(), 1) },
        0,
        "listen on SOCK_SEQPACKET fixture: {}",
        std::io::Error::last_os_error()
    );

    let peer_pid = unsafe { libc::fork() };
    assert!(
        peer_pid >= 0,
        "fork peer: {}",
        std::io::Error::last_os_error()
    );
    if peer_pid == 0 {
        let peer = owned_socket();
        let connected = unsafe {
            libc::connect(
                peer.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                address_length,
            )
        };
        if connected != 0 || write_byte(peer.as_raw_fd(), 0x47).is_err() {
            unsafe { libc::_exit(90) };
        }
        match read_byte(peer.as_raw_fd()) {
            Ok(0x50) => unsafe { libc::_exit(0) },
            _ => unsafe { libc::_exit(91) },
        }
    }

    let accepted_fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    assert!(
        accepted_fd >= 0,
        "accept SOCK_SEQPACKET peer: {}",
        std::io::Error::last_os_error()
    );
    let accepted = unsafe { OwnedFd::from_raw_fd(accepted_fd) };

    let mut credentials = MaybeUninit::<libc::ucred>::zeroed();
    let mut credentials_length = mem::size_of::<libc::ucred>() as libc::socklen_t;
    let credentials_result = unsafe {
        libc::getsockopt(
            accepted.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut credentials_length,
        )
    };
    assert_eq!(
        credentials_result,
        0,
        "SO_PEERCRED on accepted SOCK_SEQPACKET peer: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(
        credentials_length as usize,
        mem::size_of::<libc::ucred>(),
        "SO_PEERCRED returned an unexpected credential size"
    );
    let credentials = unsafe { credentials.assume_init() };
    assert_eq!(credentials.pid, peer_pid, "SO_PEERCRED peer PID mismatch");
    assert_eq!(
        credentials.uid,
        unsafe { libc::geteuid() },
        "SO_PEERCRED peer UID mismatch"
    );
    assert_eq!(
        read_byte(accepted.as_raw_fd()).expect("receive peer packet"),
        0x47,
        "SOCK_SEQPACKET payload mismatch"
    );
    write_byte(accepted.as_raw_fd(), 0x50).expect("acknowledge peer packet");

    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(peer_pid, &mut status, 0) }, peer_pid);
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 0);
}

fn owned_socket() -> OwnedFd {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    assert!(
        fd >= 0,
        "create SOCK_SEQPACKET socket: {}",
        std::io::Error::last_os_error()
    );
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn unix_socket_address(path: &Path) -> (libc::sockaddr_un, libc::socklen_t) {
    let bytes = path.as_os_str().as_bytes();
    let mut address = unsafe { mem::zeroed::<libc::sockaddr_un>() };
    assert!(
        bytes.len() < address.sun_path.len(),
        "seqpacket fixture socket path is too long"
    );
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *source as libc::c_char;
    }
    let length = mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    (address, length as libc::socklen_t)
}

fn write_byte(fd: RawFd, value: u8) -> std::io::Result<()> {
    let result = unsafe { libc::write(fd, (&value as *const u8).cast(), 1) };
    if result == 1 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn read_byte(fd: RawFd) -> std::io::Result<u8> {
    let mut value = 0u8;
    let result = unsafe { libc::read(fd, (&mut value as *mut u8).cast(), 1) };
    if result == 1 {
        Ok(value)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

struct CgroupFixture {
    root: PathBuf,
    armed: bool,
}

impl CgroupFixture {
    fn new(root: PathBuf) -> Self {
        Self { root, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CgroupFixture {
    fn drop(&mut self) {
        if !self.armed || !self.root.exists() {
            return;
        }
        let candidate = self.root.join("candidate-0");
        let _ = write_control(&candidate.join("cgroup.kill"), "1\n");
        let _ = wait_for_populated_zero(&candidate, Duration::from_secs(5));
        for path in [
            candidate.join("control"),
            candidate.join("payload"),
            candidate,
            self.root.clone(),
        ] {
            let _ = fs::remove_dir(path);
        }
    }
}

fn prove_delegated_domain_cgroup(delegated_root: &Path) {
    assert!(
        delegated_root.is_absolute(),
        "delegated cgroup root must be absolute"
    );
    let metadata = fs::symlink_metadata(delegated_root).expect("inspect delegated cgroup root");
    assert!(metadata.file_type().is_dir());
    assert!(!metadata.file_type().is_symlink());
    assert_eq!(
        metadata.uid(),
        unsafe { libc::geteuid() },
        "delegated cgroup root is not owned by the runner EUID"
    );
    assert_cgroup2(delegated_root);
    assert_domain_cgroup(delegated_root);
    assert_token(&delegated_root.join("cgroup.controllers"), "pids");
    write_control(&delegated_root.join("cgroup.subtree_control"), "+pids\n")
        .expect("enable pids at delegated root");
    assert_token(&delegated_root.join("cgroup.subtree_control"), "pids");

    let fixture_root = delegated_root.join(format!("lterm-g003-gate-{}", std::process::id()));
    fs::create_dir(&fixture_root).expect("create Gate P cgroup fixture");
    let mut cleanup = CgroupFixture::new(fixture_root.clone());
    assert_domain_cgroup(&fixture_root);
    assert_token(&fixture_root.join("cgroup.controllers"), "pids");
    write_control(&fixture_root.join("cgroup.subtree_control"), "+pids\n")
        .expect("enable pids at fixture root");
    assert_token(&fixture_root.join("cgroup.subtree_control"), "pids");

    let candidate = fixture_root.join("candidate-0");
    fs::create_dir(&candidate).expect("create candidate domain cgroup");
    assert_domain_cgroup(&candidate);
    assert_token(&candidate.join("cgroup.controllers"), "pids");
    write_control(&candidate.join("cgroup.subtree_control"), "+pids\n")
        .expect("enable pids at candidate parent");
    assert_token(&candidate.join("cgroup.subtree_control"), "pids");

    let control = candidate.join("control");
    let payload = candidate.join("payload");
    fs::create_dir(&control).expect("create control leaf cgroup");
    fs::create_dir(&payload).expect("create payload leaf cgroup");
    assert_domain_cgroup(&control);
    assert_domain_cgroup(&payload);
    write_control(&payload.join("pids.max"), "256\n").expect("set payload pids.max");
    assert_eq!(
        read_control(&payload.join("pids.max")).trim(),
        "256",
        "pids.max readback mismatch"
    );

    let mut release_pipe = [-1; 2];
    let mut ready_pipe = [-1; 2];
    assert_eq!(
        unsafe { libc::pipe2(release_pipe.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    assert_eq!(
        unsafe { libc::pipe2(ready_pipe.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let child_pid = unsafe { libc::fork() };
    assert!(child_pid >= 0, "fork cgroup fixture process");
    if child_pid == 0 {
        unsafe {
            libc::close(release_pipe[1]);
            libc::close(ready_pipe[0]);
        }
        if read_byte(release_pipe[0]).is_err() {
            unsafe { libc::_exit(92) };
        }
        let descendant = unsafe { libc::fork() };
        if descendant < 0 {
            unsafe { libc::_exit(93) };
        }
        if descendant == 0 {
            unsafe {
                libc::close(ready_pipe[1]);
                loop {
                    libc::pause();
                }
            }
        }
        if write_byte(ready_pipe[1], 0x52).is_err() {
            unsafe { libc::_exit(94) };
        }
        unsafe {
            loop {
                libc::pause();
            }
        }
    }

    unsafe {
        libc::close(release_pipe[0]);
        libc::close(ready_pipe[1]);
    }
    write_control(&payload.join("cgroup.procs"), &format!("{child_pid}\n"))
        .expect("place fixture process in payload cgroup");
    write_byte(release_pipe[1], 0x47).expect("release cgroup fixture child");
    assert_eq!(
        read_byte(ready_pipe[0]).expect("wait for cgroup descendant"),
        0x52
    );
    unsafe {
        libc::close(release_pipe[1]);
        libc::close(ready_pipe[0]);
    }

    let members = read_control(&payload.join("cgroup.procs"));
    let member_pids: Vec<&str> = members.split_ascii_whitespace().collect();
    let child_pid_text = child_pid.to_string();
    assert!(
        member_pids.iter().any(|member| *member == child_pid_text),
        "placed process is absent from payload cgroup"
    );
    assert_eq!(
        member_pids.len(),
        2,
        "payload fixture did not contain the expected recursive process topology"
    );

    write_control(&candidate.join("cgroup.kill"), "1\n")
        .expect("recursively kill candidate cgroup");
    let mut child_status = 0;
    assert_eq!(
        unsafe { libc::waitpid(child_pid, &mut child_status, 0) },
        child_pid
    );
    assert!(libc::WIFSIGNALED(child_status));
    assert_eq!(libc::WTERMSIG(child_status), libc::SIGKILL);
    wait_for_populated_zero(&candidate, Duration::from_secs(10))
        .expect("observe recursive cgroup populated 0");

    fs::remove_dir(&control).expect("remove empty control leaf");
    fs::remove_dir(&payload).expect("remove empty payload leaf");
    fs::remove_dir(&candidate).expect("remove empty candidate parent");
    fs::remove_dir(&fixture_root).expect("remove empty Gate P hierarchy");
    cleanup.disarm();
}

fn assert_cgroup2(path: &Path) {
    let path = CString::new(path.as_os_str().as_bytes()).expect("cgroup path contains NUL");
    let mut stats = MaybeUninit::<libc::statfs>::zeroed();
    assert_eq!(
        unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) },
        0,
        "statfs delegated cgroup root: {}",
        std::io::Error::last_os_error()
    );
    let stats = unsafe { stats.assume_init() };
    assert_eq!(
        stats.f_type, CGROUP2_SUPER_MAGIC,
        "delegated root is not cgroup v2"
    );
}

fn assert_domain_cgroup(path: &Path) {
    assert_eq!(
        read_control(&path.join("cgroup.type")).trim(),
        "domain",
        "threaded or non-domain cgroup is unsupported"
    );
}

fn assert_token(path: &Path, expected: &str) {
    let contents = read_control(path);
    assert!(
        contents
            .split_ascii_whitespace()
            .any(|token| token == expected),
        "required cgroup token {expected} is absent"
    );
}

fn read_control(path: &Path) -> String {
    fs::read_to_string(path).expect("read required cgroup control file")
}

fn write_control(path: &Path, value: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.write_all(value.as_bytes())
}

fn wait_for_populated_zero(path: &Path, timeout: Duration) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let events = fs::read_to_string(path.join("cgroup.events"))?;
        if events.lines().any(|line| line.trim() == "populated 0") {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "cgroup did not reach populated 0",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn prove_pinned_bwrap_namespaces() {
    let bwrap_path = Path::new("/usr/bin/bwrap");
    let path_metadata = fs::symlink_metadata(bwrap_path).expect("inspect exact /usr/bin/bwrap");
    assert!(path_metadata.file_type().is_file());
    assert!(!path_metadata.file_type().is_symlink());
    assert_eq!(path_metadata.uid(), 0, "/usr/bin/bwrap is not root-owned");
    assert_eq!(
        path_metadata.mode() & 0o022,
        0,
        "/usr/bin/bwrap is group/other writable"
    );

    let pinned = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(bwrap_path)
        .expect("open exact /usr/bin/bwrap no-follow");
    let pinned_metadata = pinned.metadata().expect("inspect pinned bwrap object");
    assert!(pinned_metadata.file_type().is_file());
    assert_eq!(pinned_metadata.dev(), path_metadata.dev());
    assert_eq!(pinned_metadata.ino(), path_metadata.ino());
    drop(pinned);

    let parent_namespaces = ["user", "pid", "net", "ipc", "uts"].map(|name| {
        (
            name,
            fs::read_link(format!("/proc/self/ns/{name}"))
                .expect("read parent namespace identity")
                .into_os_string(),
        )
    });
    let data = tempfile::tempdir().expect("create managed launch registry fixture");
    fs::set_permissions(data.path(), fs::Permissions::from_mode(0o700))
        .expect("secure managed launch registry fixture");

    let namespace_probe = r#"
set -eu
test "$(readlink /proc/self/ns/user)" != "$G003_PARENT_USER_NS"
test "$(readlink /proc/self/ns/pid)" != "$G003_PARENT_PID_NS"
test "$(readlink /proc/self/ns/net)" != "$G003_PARENT_NET_NS"
test "$(readlink /proc/self/ns/ipc)" != "$G003_PARENT_IPC_NS"
test "$(readlink /proc/self/ns/uts)" != "$G003_PARENT_UTS_NS"
test "$$" -ne 1
test ! -e /sys/fs/cgroup/cgroup.controllers
"#;

    let mut command = Command::new(env!("CARGO_BIN_EXE_lterm"));
    command
        .arg(INTERNAL_MANAGED_LAUNCH_ARG)
        .arg(bwrap_path)
        .args([
            "--unshare-user",
            "--unshare-pid",
            "--unshare-net",
            "--unshare-ipc",
            "--unshare-uts",
            "--disable-userns",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
        ]);
    for (name, identity) in parent_namespaces {
        command
            .arg("--setenv")
            .arg(format!("G003_PARENT_{}_NS", name.to_ascii_uppercase()))
            .arg(identity);
    }
    command.args([
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/bin",
        "/bin",
        "--symlink",
        "usr/sbin",
        "/sbin",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib64",
        "/lib64",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--chdir",
        "/",
        "/usr/bin/sh",
        "-ceu",
        namespace_probe,
    ]);
    let output = command
        .env("LTERM_INTERNAL_TEST_MODE", "1")
        .env("LTERM_DATA_DIR", data.path())
        .output()
        .expect("run pinned bwrap through managed launch gate");
    assert!(
        output.status.success(),
        "pinned managed bwrap namespace probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
