#[cfg(target_os = "linux")]
use std::fs;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ProcessIdentity {
    pub(crate) pid: u32,
    pub(crate) start_marker: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessSnapshot {
    pub(crate) identity: ProcessIdentity,
    pub(crate) parent_pid: u32,
}

pub(crate) trait ProcessInspector {
    fn snapshot(&self, pid: u32) -> Option<ProcessSnapshot>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SystemProcessInspector;

impl ProcessInspector for SystemProcessInspector {
    fn snapshot(&self, pid: u32) -> Option<ProcessSnapshot> {
        system_process_snapshot(pid)
    }
}

pub(crate) fn process_start_identity(pid: u32) -> Option<String> {
    system_process_snapshot(pid).map(|snapshot| snapshot.identity.start_marker)
}

#[cfg(target_os = "macos")]
fn system_process_snapshot(pid: u32) -> Option<ProcessSnapshot> {
    let raw_pid = libc::c_int::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let size_i32 = libc::c_int::try_from(size).ok()?;
    let read = unsafe {
        libc::proc_pidinfo(
            raw_pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_i32,
        )
    };
    if usize::try_from(read).ok()? != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let reported_pid = info.pbi_pid;
    if reported_pid != pid {
        return None;
    }
    Some(ProcessSnapshot {
        identity: ProcessIdentity {
            pid,
            start_marker: format!(
                "macos:{}:{}:{}",
                info.pbi_pid, info.pbi_start_tvsec, info.pbi_start_tvusec
            ),
        },
        parent_pid: info.pbi_ppid,
    })
}

#[cfg(target_os = "linux")]
fn system_process_snapshot(pid: u32) -> Option<ProcessSnapshot> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_linux_process_stat(pid, &stat)
}

#[cfg(target_os = "linux")]
fn parse_linux_process_stat(pid: u32, stat: &str) -> Option<ProcessSnapshot> {
    let (reported_pid, rest) = stat.split_once(" (")?;
    if reported_pid.parse::<u32>().ok()? != pid {
        return None;
    }
    // The command name may itself contain `)` characters, so split at the final
    // delimiter before the fixed-position fields.
    let after_comm = rest.rsplit_once(") ")?.1;
    let fields: Vec<_> = after_comm.split_whitespace().collect();
    let parent_pid = fields.get(1)?.parse::<u32>().ok()?;
    let start_ticks = *fields.get(19)?;
    Some(ProcessSnapshot {
        identity: ProcessIdentity {
            pid,
            start_marker: format!("linux:{pid}:{start_ticks}"),
        },
        parent_pid,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn system_process_snapshot(_pid: u32) -> Option<ProcessSnapshot> {
    None
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_stat_parser_reads_parent_and_birth_marker_from_one_record() {
        let mut tail = vec!["S".to_string(), "41".to_string()];
        tail.extend((0..17).map(|value| value.to_string()));
        tail.push("987654".to_string());
        let stat = format!("42 (command ) with parens) {}", tail.join(" "));
        let snapshot = super::parse_linux_process_stat(42, &stat).expect("valid stat record");
        assert_eq!(snapshot.parent_pid, 41);
        assert_eq!(snapshot.identity.start_marker, "linux:42:987654");
    }
}
