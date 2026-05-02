use crate::client;
use crate::client::AttachStdinEof;
use crate::paths;
use crate::protocol::SessionInfo;
use crate::sanitize;
use crate::server;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::raw::c_int;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Default, Serialize, Deserialize)]
struct CompatStore {
    panes: HashMap<String, CompatPane>,
    waits: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompatPane {
    pane_id: String,
    session_name: String,
    cmux_surface_id: Option<String>,
}

pub fn ensure_shim() -> Result<PathBuf> {
    let shim_dir = paths::shim_dir()?;
    let tmux_path = shim_dir.join("tmux");
    let lterm = std::env::current_exe().context("resolve current executable")?;
    let lterm = lterm
        .to_str()
        .context("lterm executable path must be valid UTF-8")?;
    let quoted_lterm = shlex::try_quote(lterm).context("quote lterm executable path")?;
    let script = format!("#!/bin/sh\nexec {quoted_lterm} tmux-compat \"$@\"\n");
    fs::write(&tmux_path, script).with_context(|| format!("write {}", tmux_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmux_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tmux_path, perms)?;
    }
    Ok(shim_dir)
}

pub fn install_shim() -> Result<()> {
    let shim_dir = ensure_shim()?;
    println!("{}", shim_dir.display());
    Ok(())
}

pub fn print_env_exports() -> Result<()> {
    ensure_shim()?;
    let shim = paths::shim_dir()?;
    let socket = paths::socket_path()?;
    let tmux = server::fake_tmux_value()?;
    println!(
        "export LTERM_SOCKET={}",
        quote(&socket.display().to_string())
    );
    println!("export TMUX={}", quote(&tmux));
    println!("export TMUX_PANE=${{TMUX_PANE:-%0}}");
    println!("export PATH={}:$PATH", quote(&shim.display().to_string()));
    Ok(())
}

pub fn run_tmux_compat(raw_args: Vec<String>) -> Result<i32> {
    let args = strip_global_flags(raw_args)?;
    if args.is_empty() {
        return Ok(0);
    }
    let cmd = args[0].as_str();
    let rest = &args[1..];
    match cmd {
        "-V" | "version" => {
            println!("tmux 3.5a (light-terminal compat)");
            Ok(0)
        }
        "new" | "new-session" => new_session(rest),
        "attach" | "attach-session" | "a" => attach_session(rest),
        "has-session" => has_session(rest),
        "list-sessions" | "ls" => list_sessions(rest),
        "kill-session" => kill_session(rest),
        "split-window" | "splitw" => split_window(rest),
        "list-panes" | "lsp" => list_panes(rest),
        "display-message" | "display" => display_message(rest),
        "capture-pane" | "capturep" => capture_pane(rest),
        "send-keys" | "send" => send_keys(rest),
        "kill-pane" | "killp" => kill_pane(rest),
        "resize-pane" | "resizep" => resize_pane(rest),
        "select-pane" | "selectp" => Ok(0),
        "select-layout" => Ok(0),
        "set-option" | "set" | "setw" | "set-window-option" => Ok(0),
        "show-options" | "show" | "show-option" | "showw" | "show-window-option" => {
            show_option(rest)
        }
        "display-popup" | "popup" => display_popup(rest),
        "wait-for" | "wait" => wait_for(rest),
        "load-buffer" | "loadb" => load_buffer(rest),
        "save-buffer" | "saveb" => save_buffer(rest),
        "paste-buffer" | "pasteb" => paste_buffer(rest),
        "set-environment" | "setenv" | "show-environment" | "showenv" => Ok(0),
        unknown => bail!(
            "unsupported tmux command in lterm compat: {unknown} {}",
            rest.join(" ")
        ),
    }
}

fn strip_global_flags(raw_args: Vec<String>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw_args.len() {
        let arg = &raw_args[i];
        match arg.as_str() {
            "-L" | "-S" | "-f" => {
                if i + 1 >= raw_args.len() {
                    bail!("missing value for global tmux flag {arg}");
                }
                if matches!(arg.as_str(), "-L" | "-S") {
                    bail!("global tmux server selection flag {arg} is not supported by lterm");
                }
                i += 2;
            }
            "-q" | "-u" | "-2" | "-CC" | "-N" => i += 1,
            _ => {
                out.extend_from_slice(&raw_args[i..]);
                break;
            }
        }
    }
    Ok(out)
}

fn new_session(args: &[String]) -> Result<i32> {
    let mut detached = false;
    let mut name = None;
    let mut cwd = None;
    let mut command = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-d" => {
                detached = true;
                i += 1;
            }
            "-s" => {
                name = args.get(i + 1).cloned();
                i += 2;
            }
            "-c" => {
                cwd = args.get(i + 1).cloned();
                i += 2;
            }
            "--" => {
                command.extend_from_slice(&args[i + 1..]);
                break;
            }
            flag if flag.starts_with('-') => i += flag_arg_width(flag, args, i),
            _ => {
                command.extend_from_slice(&args[i..]);
                break;
            }
        }
    }
    let command = tmux_shell_command(&command)?;
    let info = client::new_session(name, command, cwd, HashMap::new(), true)?;
    remember_pane(&info, None)?;
    if detached {
        Ok(0)
    } else {
        client::attach(&info.name, true, AttachStdinEof::KeepAttached)?;
        Ok(0)
    }
}

fn attach_session(args: &[String]) -> Result<i32> {
    let target = parse_target(args).unwrap_or_else(default_target);
    client::attach(&target, true, AttachStdinEof::Detach)?;
    Ok(0)
}

fn has_session(args: &[String]) -> Result<i32> {
    let target = parse_target(args).unwrap_or_else(default_target);
    match client::info(&target) {
        Ok(_) => Ok(0),
        Err(_) => Ok(1),
    }
}

fn list_sessions(args: &[String]) -> Result<i32> {
    let format = parse_format(args).unwrap_or_else(|| "#{session_name}".to_string());
    for pane in client::list_sessions()? {
        // The daemon stores aliases; collapse by pane id.
        if pane.name.starts_with('%') || pane.name == pane.id {
            continue;
        }
        println!("{}", expand_format(&format, &pane));
    }
    Ok(0)
}

fn kill_session(args: &[String]) -> Result<i32> {
    let target = parse_target(args).unwrap_or_else(default_target);
    client::kill(&target)?;
    Ok(0)
}

fn split_window(args: &[String]) -> Result<i32> {
    let mut direction = "right";
    let mut print = false;
    let mut format = "#{pane_id}".to_string();
    let mut target = None;
    let mut cwd = None;
    let mut detached = false;
    let mut command = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" => {
                direction = "right";
                i += 1;
            }
            "-v" => {
                direction = "down";
                i += 1;
            }
            "-d" => {
                detached = true;
                i += 1;
            }
            "-P" => {
                print = true;
                i += 1;
            }
            "-F" => {
                format = args.get(i + 1).cloned().unwrap_or(format);
                i += 2;
            }
            "-t" => {
                target = args.get(i + 1).cloned();
                i += 2;
            }
            "-c" => {
                cwd = args.get(i + 1).cloned();
                i += 2;
            }
            "--" => {
                command.extend_from_slice(&args[i + 1..]);
                break;
            }
            flag if flag.starts_with('-') => i += flag_arg_width(flag, args, i),
            _ => {
                command.extend_from_slice(&args[i..]);
                break;
            }
        }
    }
    let command = tmux_shell_command(&command)?;
    let info = client::new_session(None, command, cwd, HashMap::new(), true)?;

    let cmux_surface = if !detached {
        open_cmux_split(direction, &info).ok().flatten()
    } else {
        None
    };
    remember_pane(&info, cmux_surface)?;

    if print {
        println!("{}", expand_format(&format, &info));
    } else if target.is_some() {
        // tmux is quiet by default. target is parsed for compatibility only.
    }
    Ok(0)
}

fn list_panes(args: &[String]) -> Result<i32> {
    let format = parse_format(args).unwrap_or_else(|| "#{pane_id}".to_string());
    if let Some(target) = parse_target(args) {
        let pane = client::info(&target)?;
        println!("{}", expand_format(&format, &pane));
        return Ok(0);
    }
    let mut seen = HashSet::new();
    for pane in client::list_sessions()? {
        if !seen.insert(pane.pane_id.clone()) {
            continue;
        }
        println!("{}", expand_format(&format, &pane));
    }
    Ok(0)
}

fn display_message(args: &[String]) -> Result<i32> {
    let mut print = false;
    let mut target = None;
    let mut explicit_target = false;
    let mut message = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                print = true;
                i += 1;
            }
            "-t" => {
                target = args.get(i + 1).cloned();
                explicit_target = true;
                i += 2;
            }
            "-F" => {
                message = args.get(i + 1).cloned();
                i += 2;
            }
            "--" => {
                message = Some(args[i + 1..].join(" "));
                break;
            }
            flag if flag.starts_with('-') => i += flag_arg_width(flag, args, i),
            _ => {
                message = Some(args[i..].join(" "));
                break;
            }
        }
    }
    let target = target.unwrap_or_else(default_target);
    let info = match client::info(&target) {
        Ok(info) => info,
        Err(err) if explicit_target => return Err(err),
        Err(_) => client::list_sessions()
            .and_then(|v| v.into_iter().next().ok_or_else(|| anyhow!("no panes")))?,
    };
    let msg = message.unwrap_or_default();
    let expanded = expand_format(&msg, &info);
    if print {
        println!("{expanded}");
    } else {
        eprintln!("{expanded}");
    }
    Ok(0)
}

fn capture_pane(args: &[String]) -> Result<i32> {
    let mut target = None;
    let mut print = false;
    let mut start = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                print = true;
                i += 1;
            }
            "-t" => {
                target = args.get(i + 1).cloned();
                i += 2;
            }
            "-S" => {
                start = args.get(i + 1).and_then(|s| s.parse::<i32>().ok());
                i += 2;
            }
            "-e" | "-J" => i += 1,
            flag if flag.starts_with('-') => i += flag_arg_width(flag, args, i),
            _ => i += 1,
        }
    }
    let target = target.unwrap_or_else(default_target);
    let text = client::capture(&target, start)?;
    if print {
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    } else {
        fs::write(paths::buffer_path()?, text)?;
    }
    Ok(0)
}

fn send_keys(args: &[String]) -> Result<i32> {
    let mut target = None;
    let mut literal = false;
    let mut keys = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-t" => {
                target = args.get(i + 1).cloned();
                i += 2;
            }
            "-l" => {
                literal = true;
                i += 1;
            }
            "--" => {
                keys.extend_from_slice(&args[i + 1..]);
                break;
            }
            flag if flag.starts_with('-') => i += flag_arg_width(flag, args, i),
            _ => {
                keys.extend_from_slice(&args[i..]);
                break;
            }
        }
    }
    let target = target.unwrap_or_else(default_target);
    let bytes = keys_to_bytes(&keys, literal);
    client::send(&target, bytes)?;
    Ok(0)
}

fn kill_pane(args: &[String]) -> Result<i32> {
    let target = parse_target(args).unwrap_or_else(default_target);
    client::kill(&target)?;
    Ok(0)
}

fn resize_pane(args: &[String]) -> Result<i32> {
    let target = parse_target(args).unwrap_or_else(default_target);
    let mut rows = None;
    let mut cols = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-x" => {
                cols = args.get(i + 1).and_then(|s| s.parse::<u16>().ok());
                i += 2;
            }
            "-y" => {
                rows = args.get(i + 1).and_then(|s| s.parse::<u16>().ok());
                i += 2;
            }
            _ => i += 1,
        }
    }
    match (rows, cols) {
        (Some(0), _) | (_, Some(0)) => bail!("resize dimensions must be at least 1"),
        (Some(rows), Some(cols)) => client::resize(&target, rows, cols)?,
        (Some(rows), None) => {
            let info = client::info(&target)?;
            client::resize(&target, rows, info.cols)?;
        }
        (None, Some(cols)) => {
            let info = client::info(&target)?;
            client::resize(&target, info.rows, cols)?;
        }
        (None, None) => {}
    }
    Ok(0)
}

fn show_option(args: &[String]) -> Result<i32> {
    if args.iter().any(|a| a == "-g" || a == "-v") {
        // Return a conservative default for scripts that query options.
        println!("off");
    }
    Ok(0)
}

fn display_popup(args: &[String]) -> Result<i32> {
    let mut command = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-E" => {
                command = tmux_shell_command(&args[i + 1..])?;
                break;
            }
            "--" => {
                command = tmux_shell_command(&args[i + 1..])?;
                break;
            }
            _ => i += 1,
        }
    }
    if let Some(command) = command {
        let status = Command::new(default_shell())
            .arg("-c")
            .arg(command)
            .status()?;
        Ok(status.code().unwrap_or(1))
    } else {
        Ok(0)
    }
}

fn wait_for(args: &[String]) -> Result<i32> {
    let mut signal = false;
    let mut channel = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-S" => {
                signal = true;
                channel = args.get(i + 1).cloned();
                i += 2;
            }
            "-L" | "-U" => bail!("tmux wait-for {} is not supported by lterm", args[i]),
            other => {
                channel = Some(other.to_string());
                i += 1;
            }
        }
    }
    let channel = channel.unwrap_or_else(|| "default".to_string());
    if signal {
        update_store(|store| {
            store.waits.insert(channel);
            Ok(())
        })?;
        return Ok(0);
    }

    let deadline = Instant::now() + Duration::from_secs(60 * 60 * 24);
    let mut sleep_for = Duration::from_millis(100);
    while Instant::now() < deadline {
        let signaled = update_store(|store| {
            let signaled = store.waits.remove(&channel);
            Ok(signaled)
        })?;
        if signaled {
            return Ok(0);
        }
        thread::sleep(sleep_for);
        sleep_for = (sleep_for + Duration::from_millis(100)).min(Duration::from_secs(1));
    }
    Ok(1)
}

fn load_buffer(args: &[String]) -> Result<i32> {
    let source = args.iter().find(|a| !a.starts_with('-')).cloned();
    let mut data = Vec::new();
    if let Some(path) = source {
        if path == "-" {
            std::io::stdin().read_to_end(&mut data)?;
        } else {
            data = fs::read(path)?;
        }
    } else {
        std::io::stdin().read_to_end(&mut data)?;
    }
    fs::write(paths::buffer_path()?, data)?;
    Ok(0)
}

fn save_buffer(args: &[String]) -> Result<i32> {
    let data = read_buffer_or_empty()?;
    let dest = args.iter().find(|a| !a.starts_with('-')).cloned();
    if let Some(path) = dest {
        if path == "-" {
            std::io::stdout().write_all(&data)?;
        } else {
            fs::write(path, data)?;
        }
    } else {
        std::io::stdout().write_all(&data)?;
    }
    Ok(0)
}

fn paste_buffer(args: &[String]) -> Result<i32> {
    let target = parse_target(args).unwrap_or_else(default_target);
    let data = read_buffer_or_empty()?;
    client::send(&target, data)?;
    Ok(0)
}

fn read_buffer_or_empty() -> Result<Vec<u8>> {
    match fs::read(paths::buffer_path()?) {
        Ok(data) => Ok(data),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).context("read tmux buffer"),
    }
}

fn open_cmux_split(direction: &str, info: &SessionInfo) -> Result<Option<String>> {
    if !inside_cmux() || !client::command_exists("cmux") {
        return Ok(None);
    }
    let split_status = Command::new("cmux")
        .arg("new-split")
        .arg(direction)
        .status()
        .context("cmux new-split")?;
    if !split_status.success() {
        return Ok(None);
    }

    let surface = cmux_identify_surface().ok().flatten();
    let lterm = std::env::var("LTERM_BIN").ok().unwrap_or_else(|| {
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "lterm".to_string())
    });
    let attach_cmd = format!("exec {} attach {}\n", quote(&lterm), quote(&info.pane_id));
    let mut send = Command::new("cmux");
    if let Some(surface_id) = &surface {
        send.arg("send-surface")
            .arg("--surface")
            .arg(surface_id)
            .arg(&attach_cmd);
    } else {
        send.arg("send").arg(&attach_cmd);
    }
    let _ = send.status();
    Ok(surface)
}

fn inside_cmux() -> bool {
    if std::env::var_os("CMUX_WORKSPACE_ID").is_some()
        || std::env::var_os("CMUX_SURFACE_ID").is_some()
    {
        return true;
    }
    let Some(path) = std::env::var_os("CMUX_SOCKET_PATH").map(PathBuf::from) else {
        return false;
    };
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    !meta.file_type().is_symlink()
        && meta.file_type().is_socket()
        && meta.uid() == paths::current_euid()
}

fn cmux_identify_surface() -> Result<Option<String>> {
    let output = Command::new("cmux")
        .arg("identify")
        .arg("--json")
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    Ok(find_json_string(
        &value,
        &["surface_id", "surfaceId", "surface", "id"],
    ))
}

fn find_json_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for key in keys {
                if let Some(v) = map.get(*key).and_then(|v| v.as_str()) {
                    return Some(v.to_string());
                }
            }
            for v in map.values() {
                if let Some(found) = find_json_string(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(items) => items.iter().find_map(|v| find_json_string(v, keys)),
        _ => None,
    }
}

fn remember_pane(info: &SessionInfo, cmux_surface_id: Option<String>) -> Result<()> {
    update_store(|store| {
        store.panes.insert(
            info.pane_id.clone(),
            CompatPane {
                pane_id: info.pane_id.clone(),
                session_name: info.name.clone(),
                cmux_surface_id,
            },
        );
        Ok(())
    })
}

fn load_store() -> Result<CompatStore> {
    let path = paths::store_path()?;
    if !path.exists() {
        return Ok(CompatStore::default());
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn save_store(store: &CompatStore) -> Result<()> {
    let path = paths::store_path()?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(store)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))
}

fn update_store<T>(f: impl FnOnce(&mut CompatStore) -> Result<T>) -> Result<T> {
    let _lock = StoreLock::acquire()?;
    let mut store = load_store()?;
    let result = f(&mut store)?;
    save_store(&store)?;
    Ok(result)
}

struct StoreLock {
    file: File,
}

impl StoreLock {
    fn acquire() -> Result<Self> {
        let path = paths::store_lock_path()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        loop {
            let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
            if rc == 0 {
                return Ok(Self { file });
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for {}", path.display());
                }
                thread::sleep(Duration::from_millis(25));
                continue;
            }
            return Err(err).with_context(|| format!("lock {}", path.display()));
        }
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = unsafe { flock(self.file.as_raw_fd(), LOCK_UN) };
    }
}

fn parse_target(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-t" {
            return args.get(i + 1).cloned();
        }
        if let Some(value) = args[i].strip_prefix("-t=") {
            return Some(value.to_string());
        }
        if args[i].starts_with("-t") && args[i].len() > 2 {
            return Some(args[i][2..].to_string());
        }
        i += 1;
    }
    None
}

fn parse_format(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-F" {
            return args.get(i + 1).cloned();
        }
        if let Some(value) = args[i].strip_prefix("-F=") {
            return Some(value.to_string());
        }
        if args[i].starts_with("-F") && args[i].len() > 2 {
            return Some(args[i][2..].to_string());
        }
        i += 1;
    }
    None
}

fn flag_arg_width(flag: &str, args: &[String], i: usize) -> usize {
    match flag {
        "-t" | "-F" | "-c" | "-s" | "-n" | "-x" | "-y" | "-l" => {
            if args.get(i + 1).is_some() {
                2
            } else {
                1
            }
        }
        _ => 1,
    }
}

fn default_target() -> String {
    std::env::var("TMUX_PANE")
        .ok()
        .or_else(|| std::env::var("LTERM_PANE").ok())
        .unwrap_or_else(|| "%0".to_string())
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

fn tmux_shell_command(args: &[String]) -> Result<Option<String>> {
    if args.is_empty() {
        Ok(None)
    } else if args.len() == 1 {
        Ok(Some(args[0].clone()))
    } else {
        Ok(Some(client::shell_join(args)?))
    }
}

pub fn quote(value: &str) -> String {
    shlex::try_quote(value)
        .expect("shell quote should be infallible for NUL-free Rust strings")
        .into_owned()
}

pub fn expand_format(format: &str, info: &SessionInfo) -> String {
    let current_command = current_command(&info.command);
    let mut out = String::new();
    let mut i = 0;
    while i < format.len() {
        let rest = &format[i..];
        if rest.starts_with("#{pane_width}") {
            out.push_str(&info.cols.to_string());
            i += "#{pane_width}".len();
        } else if rest.starts_with("#{pane_height}") {
            out.push_str(&info.rows.to_string());
            i += "#{pane_height}".len();
        } else if let Some((needle, value)) = format_replacement(rest, info, &current_command) {
            out.push_str(&sanitize::terminal_text(value));
            i += needle.len();
        } else {
            let ch = rest
                .chars()
                .next()
                .expect("format index should be on a char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn format_replacement<'a>(
    rest: &str,
    info: &'a SessionInfo,
    current_command: &'a str,
) -> Option<(&'static str, &'a str)> {
    const ACTIVE: &str = "1";
    const IN_MODE: &str = "0";
    const WIDTH: &str = "80";
    const HEIGHT: &str = "24";
    if rest.starts_with("#{pane_id}") {
        Some(("#{pane_id}", info.pane_id.as_str()))
    } else if rest.starts_with("#D") {
        Some(("#D", info.pane_id.as_str()))
    } else if rest.starts_with("#{session_name}") {
        Some(("#{session_name}", info.name.as_str()))
    } else if rest.starts_with("#S") {
        Some(("#S", info.name.as_str()))
    } else if rest.starts_with("#{pane_current_command}") {
        Some(("#{pane_current_command}", current_command))
    } else if rest.starts_with("#{pane_start_command}") {
        Some(("#{pane_start_command}", info.command.as_str()))
    } else if rest.starts_with("#{pane_current_path}") {
        Some(("#{pane_current_path}", info.cwd.as_str()))
    } else if rest.starts_with("#{pane_active}") {
        Some(("#{pane_active}", ACTIVE))
    } else if rest.starts_with("#{pane_in_mode}") {
        Some(("#{pane_in_mode}", IN_MODE))
    } else if rest.starts_with("#{window_width}") {
        Some(("#{window_width}", WIDTH))
    } else if rest.starts_with("#{window_height}") {
        Some(("#{window_height}", HEIGHT))
    } else {
        None
    }
}

fn current_command(command: &str) -> String {
    command
        .split_whitespace()
        .next()
        .map(|s| s.rsplit('/').next().unwrap_or(s).to_string())
        .unwrap_or_else(|| "sh".to_string())
}

const LOCK_EX: c_int = 2;
const LOCK_NB: c_int = 4;
const LOCK_UN: c_int = 8;

unsafe extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
}

pub fn keys_to_bytes(keys: &[String], literal: bool) -> Vec<u8> {
    if literal {
        return keys.concat().into_bytes();
    }
    let mut out = Vec::new();
    for key in keys {
        match key.as_str() {
            "C-m" | "Enter" | "enter" | "Return" => out.push(b'\r'),
            "C-j" => out.push(b'\n'),
            "C-c" => out.push(0x03),
            "C-d" => out.push(0x04),
            "C-z" => out.push(0x1a),
            "Tab" | "tab" => out.push(b'\t'),
            "Space" | "space" => out.push(b' '),
            "Escape" | "Esc" | "escape" => out.push(0x1b),
            "BSpace" | "Backspace" | "backspace" => out.push(0x7f),
            "Up" | "up" => out.extend_from_slice(b"\x1b[A"),
            "Down" | "down" => out.extend_from_slice(b"\x1b[B"),
            "Right" | "right" => out.extend_from_slice(b"\x1b[C"),
            "Left" | "left" => out.extend_from_slice(b"\x1b[D"),
            text => out.extend_from_slice(text.as_bytes()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_keys() {
        let keys = vec![
            "echo".to_string(),
            "Space".to_string(),
            "ok".to_string(),
            "C-m".to_string(),
        ];
        assert_eq!(keys_to_bytes(&keys, false), b"echo ok\r");
        assert_eq!(keys_to_bytes(&["a".into(), "b".into()], true), b"ab");
        assert_eq!(keys_to_bytes(&["C-j".into()], false), b"\n");
    }

    #[test]
    fn expands_tmux_formats() {
        let info = SessionInfo {
            id: "id".into(),
            name: "s".into(),
            pane_id: "%1".into(),
            command: "codex --ask".into(),
            cwd: "/tmp".into(),
            created_unix_ms: 0,
            alive: true,
            exit_code: None,
            rows: 24,
            cols: 80,
            process_id: None,
            process_group_id: None,
        };
        assert_eq!(
            expand_format("#{pane_id} #S #{pane_current_command}", &info),
            "%1 s codex"
        );
    }
}
