use crate::client;
use crate::client::AttachStdinEof;
use crate::paths;
use crate::protocol::{MAX_SEND_DATA_BYTES, SessionInfo};
use crate::sanitize;
use crate::server;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const WAIT_GENERATION_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;
const WAIT_GENERATION_MAX_CHANNELS: usize = 4_096;
const WAIT_GENERATION_TOUCH_INTERVAL_SECS: u64 = 30;
const MANAGED_ATTACH_ENV: &str = "LTERM_CMUX_MANAGED_ATTACH";
const MANAGED_ATTACH_LEASE_TTL_SECS: u64 = 120;
const MANAGED_ATTACH_RENEW_SECS: u64 = 30;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct CompatStore {
    panes: HashMap<String, CompatPane>,
    wait_generations: HashMap<String, u64>,
    wait_generation_touched_secs: HashMap<String, u64>,
    managed_attaches: HashMap<String, ManagedAttachLease>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompatPane {
    pane_id: String,
    session_name: String,
    cmux_surface_id: Option<String>,
    cmux_workspace_id: Option<String>,
    cmux_window_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedAttachLease {
    pane_id: String,
    token: String,
    pid: u32,
    process_start_id: Option<String>,
    cmux_surface_id: Option<String>,
    cmux_workspace_id: Option<String>,
    cmux_window_id: Option<String>,
    updated_secs: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CapturePaneArgs {
    target: Option<String>,
    print: bool,
    start: Option<i32>,
    end: Option<i32>,
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
    let existing = match fs::read(&tmux_path) {
        Ok(bytes) => Some(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("read {}", tmux_path.display())),
    };
    if existing.as_deref() != Some(script.as_bytes()) {
        fs::write(&tmux_path, script).with_context(|| format!("write {}", tmux_path.display()))?;
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvShell {
    Posix,
    Fish,
}

pub fn print_env_exports(shell: EnvShell) -> Result<()> {
    ensure_shim()?;
    let shim = paths::shim_dir()?;
    let socket = paths::socket_path()?;
    let tmux = server::fake_tmux_value()?;
    match shell {
        EnvShell::Posix => {
            println!(
                "export LTERM_SOCKET={}",
                quote(&socket.display().to_string())
            );
            println!("export TMUX={}", quote(&tmux));
            println!("export TMUX_PANE=${{TMUX_PANE:-%0}}");
            println!("export PATH={}:$PATH", quote(&shim.display().to_string()));
        }
        EnvShell::Fish => {
            let shim = fish_quote(&shim.display().to_string());
            println!(
                "set -gx LTERM_SOCKET {}",
                fish_quote(&socket.display().to_string())
            );
            println!("set -gx TMUX {}", fish_quote(&tmux));
            println!("string length -q -- \"$TMUX_PANE\"; or set -gx TMUX_PANE '%0'");
            println!("contains -- {shim} $PATH; or set -gx PATH {shim} $PATH");
        }
    }
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
        "-V" | "--version" | "version" => {
            println!("tmux 3.5a (light-terminal compat)");
            Ok(0)
        }
        "new" | "new-session" => new_session(rest),
        "attach" | "attach-session" | "a" => attach_session(rest),
        "has" | "has-session" => has_session(rest),
        "list-sessions" | "ls" => list_sessions(rest),
        "list-windows" | "lsw" => list_windows(rest),
        "list-clients" | "lsc" => list_clients(rest),
        "list-commands" | "lscm" => list_commands(rest),
        "kill-session" => kill_session(rest),
        "rename-session" | "rename" => rename_session(rest),
        "split-window" | "splitw" => split_window(rest),
        "list-panes" | "lsp" => list_panes(rest),
        "display-message" | "display" => display_message(rest),
        "capture-pane" | "capturep" => capture_pane(rest),
        "send-keys" | "send" => send_keys(rest),
        "kill-pane" | "killp" => kill_pane(rest),
        "resize-pane" | "resizep" => resize_pane(rest),
        "refresh-client" | "refresh" => Ok(0),
        "select-pane" | "selectp" => Ok(0),
        "select-layout" | "selectl" => Ok(0),
        "set-hook" | "seth" => set_hook(rest),
        "set-option" | "set" | "setw" | "set-window-option" => Ok(0),
        "show-options"
        | "show"
        | "show-option"
        | "showw"
        | "show-window-option"
        | "show-window-options" => show_option(rest),
        "display-popup" | "popup" => display_popup(rest),
        "run-shell" | "run" => run_shell(rest),
        "wait-for" | "wait" => wait_for(rest),
        "load-buffer" | "loadb" => load_buffer(rest),
        "save-buffer" | "saveb" => save_buffer(rest),
        "paste-buffer" | "pasteb" => paste_buffer(rest),
        "set-environment" | "setenv" | "show-environment" | "showenv" => {
            // Agent scripts commonly probe these commands; lterm has no tmux-style
            // environment store yet, so they are accepted as compatibility no-ops.
            Ok(0)
        }
        unknown => {
            debug_unsupported_command(unknown, rest);
            let command = sanitize::terminal_text(unknown);
            let args = sanitize::terminal_text(&rest.join(" "));
            bail!(
                "unsupported tmux command in lterm compat: {command} {args}. \
                 Run `lterm tmux-compat list-commands` to inspect supported commands."
            )
        }
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
    let mut print = false;
    let mut format = "#{pane_id}".to_string();
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
            "-P" => {
                print = true;
                i += 1;
            }
            "-F" => {
                format = value_for_option(args.get(i + 1).cloned(), "-F")?;
                i += 2;
            }
            "-s" => {
                name = Some(value_for_option(args.get(i + 1).cloned(), "-s")?);
                i += 2;
            }
            "-c" => {
                cwd = Some(value_for_option(args.get(i + 1).cloned(), "-c")?);
                i += 2;
            }
            "--" => {
                command.extend_from_slice(&args[i + 1..]);
                break;
            }
            flag if flag.starts_with('-') => {
                if has_flag_in_arg(flag, 'd') {
                    detached = true;
                }
                if has_flag_in_arg(flag, 'P') {
                    print = true;
                }
                if let Some((_, value)) = short_cluster_flag_value(flag, 'F', args, i) {
                    format = value_for_option(value.or_else(|| args.get(i + 1).cloned()), "-F")?;
                }
                i += flag_arg_width(flag, args, i);
            }
            _ => {
                command.extend_from_slice(&args[i..]);
                break;
            }
        }
    }
    let command = tmux_shell_command(&command)?;
    let info = client::new_session(name, command, cwd, HashMap::new(), None, true)?;
    remember_pane(&info, None)?;
    if print {
        println!("{}", expand_format(&format, &info));
    }
    if detached {
        Ok(0)
    } else {
        client::attach(&info.name, true, AttachStdinEof::KeepAttached)?;
        Ok(0)
    }
}

fn attach_session(args: &[String]) -> Result<i32> {
    let target = parse_target(args)?.unwrap_or_else(default_target);
    let target = normalize_tmux_target(&target)?;
    client::attach(target.as_ref(), true, AttachStdinEof::Detach)?;
    Ok(0)
}

fn has_session(args: &[String]) -> Result<i32> {
    let target = parse_target(args)?.unwrap_or_else(default_target);
    let target = normalize_tmux_target(&target)?;
    match client::info(target.as_ref()) {
        Ok(_) => Ok(0),
        Err(info_err) => match client::list_sessions() {
            Ok(sessions)
                if !sessions
                    .iter()
                    .any(|info| target_matches_info(target.as_ref(), info)) =>
            {
                Ok(1)
            }
            Ok(_) => Err(info_err).with_context(|| {
                format!("lterm info failed even though target {target:?} is present in list output")
            }),
            Err(_) => Err(info_err),
        },
    }
}

fn target_matches_info(target: &str, info: &SessionInfo) -> bool {
    target == info.name
        || target == info.pane_id
        || target == info.id
        || (!target.starts_with('%') && !target.is_empty() && format!("%{target}") == info.pane_id)
}

fn list_sessions(args: &[String]) -> Result<i32> {
    reject_filter(args)?;
    let format = parse_format(args).unwrap_or_else(|| "#{session_name}".to_string());
    for pane in root_session_rows()? {
        println!("{}", expand_format(&format, &pane));
    }
    Ok(0)
}

fn list_windows(args: &[String]) -> Result<i32> {
    // Observational compatibility only: lterm exposes a single synthetic
    // tmux-style window row per root session so status/HUD tools can query
    // dimensions. This is not an ownership model and must not be used as a
    // basis for window cleanup commands such as kill-window.
    reject_filter(args)?;
    let format =
        parse_format(args).unwrap_or_else(|| "#{window_index}: #{window_name}".to_string());
    if has_flag(args, "-a") {
        for pane in root_session_rows()? {
            println!("{}", expand_format(&format, &pane));
        }
    } else {
        let target = parse_target(args)?.unwrap_or_else(default_target);
        let pane = window_row_for_target(&target)?;
        println!("{}", expand_format(&format, &pane));
    }
    Ok(0)
}

fn list_clients(args: &[String]) -> Result<i32> {
    reject_filter(args)?;
    let format = parse_format(args).unwrap_or_else(|| "#{client_name}".to_string());
    let panes = if let Some(target) = parse_target(args)? {
        vec![window_row_for_target(&target)?]
    } else {
        root_session_rows()?
    };
    for pane in panes {
        if pane.attached_clients > 0 {
            for _ in 0..pane.attached_clients {
                println!("{}", expand_format(&format, &pane));
            }
        }
    }
    Ok(0)
}

fn list_commands(args: &[String]) -> Result<i32> {
    reject_filter(args)?;
    let format = parse_format(args);
    let requested = parse_command_filter(args);
    let json = has_flag(args, "--json");
    let verbose = has_flag(args, "--verbose");
    let rows: Vec<_> = SUPPORTED_COMMANDS
        .iter()
        .copied()
        .filter(|(command, alias, extra_aliases)| {
            if let Some(requested) = requested.as_deref() {
                let alias_matches = alias.is_some_and(|alias| requested == alias);
                let extra_alias_matches = extra_aliases.contains(&requested);
                requested == *command || alias_matches || extra_alias_matches
            } else {
                true
            }
        })
        .collect();
    if json {
        let json_rows: Vec<_> = rows
            .iter()
            .map(|(command, alias, extra_aliases)| {
                serde_json::json!({
                    "name": command,
                    "alias": alias.unwrap_or_default(),
                    "aliases": extra_aliases,
                    "usage": command_usage(command),
                    "support": command_support_tier(command),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_rows)?);
        return Ok(0);
    }
    if verbose && format.is_none() {
        for (command, alias, _) in rows {
            println!(
                "{}\t{}\t{}\t{}",
                sanitize::terminal_text(command),
                sanitize::terminal_text(alias.unwrap_or_default()),
                command_support_tier(command),
                sanitize::terminal_text(command_usage(command))
            );
        }
        return Ok(0);
    }
    let format = format.unwrap_or_else(|| "#{command_name}".to_string());
    for (command, alias, _) in rows {
        println!("{}", expand_command_format(&format, command, alias));
    }
    Ok(0)
}

fn root_session_rows() -> Result<Vec<SessionInfo>> {
    Ok(client::list_sessions()?
        .into_iter()
        // The daemon stores aliases; collapse by pane id.
        .filter(|pane| {
            pane.parent_pane_id.is_none() && !pane.name.starts_with('%') && pane.name != pane.id
        })
        .collect())
}

fn window_row_for_target(target: &str) -> Result<SessionInfo> {
    let mut pane = info_for_tmux_target(target)?;
    let rows = client::list_sessions()?;
    let mut seen = HashSet::new();
    while let Some(parent_session_id) = pane.parent_session_id.clone() {
        if !seen.insert(parent_session_id.clone()) {
            break;
        }
        let Some(parent) = rows
            .iter()
            .find(|candidate| {
                candidate.id == parent_session_id
                    && candidate.parent_pane_id.is_none()
                    && !candidate.name.starts_with('%')
                    && candidate.name != candidate.id
            })
            .or_else(|| {
                rows.iter()
                    .find(|candidate| candidate.id == parent_session_id)
            })
        else {
            break;
        };
        pane = parent.clone();
    }
    if let Some(canonical) = rows.iter().find(|candidate| {
        candidate.id == pane.id
            && candidate.parent_pane_id.is_none()
            && !candidate.name.starts_with('%')
            && candidate.name != candidate.id
    }) {
        pane = canonical.clone();
    }
    Ok(pane)
}

fn window_pane_rows_for_target(target: &str) -> Result<Vec<SessionInfo>> {
    let root = window_row_for_target(target)?;
    let mut panes = vec![root.clone()];
    let mut seen = HashSet::from([root.id.clone()]);
    let rows = client::list_sessions()?;
    let mut pending = vec![root.id.clone()];
    while let Some(parent_id) = pending.pop() {
        for child in rows.iter().filter(|candidate| {
            candidate
                .parent_session_id
                .as_deref()
                .is_some_and(|id| id == parent_id)
        }) {
            if seen.insert(child.id.clone()) {
                pending.push(child.id.clone());
                panes.push(child.clone());
            }
        }
    }
    panes.sort_by_key(|pane| pane_number(&pane.pane_id).unwrap_or(usize::MAX));
    Ok(panes)
}

fn pane_number(pane_id: &str) -> Option<usize> {
    pane_id.strip_prefix('%')?.parse().ok()
}

fn kill_session(args: &[String]) -> Result<i32> {
    let target = parse_target(args)?.unwrap_or_else(default_target);
    let target = normalize_tmux_target(&target)?;
    kill_session_with_cmux_cleanup(target.as_ref())?;
    Ok(0)
}

fn kill_pane_with_cmux_cleanup(target: &str) -> Result<()> {
    let before = client::info(target).ok();
    let pane_id = before
        .as_ref()
        .map(|info| info.pane_id.clone())
        .or_else(|| target.strip_prefix('%').map(|digits| format!("%{digits}")));
    let cmux_surface = pane_id
        .as_deref()
        .and_then(stored_cmux_surface_for_pane_best_effort);
    client::kill(target)?;
    if let Some(pane_id) = pane_id.as_deref() {
        forget_pane_best_effort(pane_id);
    }
    if let Some(surface) = cmux_surface.as_ref() {
        close_cmux_surface_best_effort("cmux close-surface for killed lterm pane", surface);
    }
    Ok(())
}

fn kill_session_with_cmux_cleanup(target: &str) -> Result<()> {
    let (kill_target, panes_before) = match window_pane_rows_for_target(target) {
        Ok(panes_before) => {
            let kill_target = panes_before
                .first()
                .map(|pane| pane.name.clone())
                .unwrap_or_else(|| target.to_string());
            (kill_target, panes_before)
        }
        Err(err) => {
            eprintln!(
                "warning: tmux compat pane enumeration failed for {}: {}",
                sanitize::terminal_text(target),
                sanitize::terminal_text(&err.to_string())
            );
            let fallback = client::info(target).ok().into_iter().collect();
            (target.to_string(), fallback)
        }
    };
    let mut cmux_surfaces = HashSet::new();
    for pane in &panes_before {
        if let Some(surface) = stored_cmux_surface_for_pane_best_effort(&pane.pane_id) {
            cmux_surfaces.insert(surface);
        }
    }

    client::kill(&kill_target)?;

    for pane in panes_before {
        forget_pane_best_effort(&pane.pane_id);
    }
    for surface in &cmux_surfaces {
        close_cmux_surface_best_effort("cmux close-surface for killed lterm session", surface);
    }
    Ok(())
}

/// Implements the tmux-compatible `rename-session [-t target] new-name` shim.
fn rename_session(args: &[String]) -> Result<i32> {
    let mut target = None;
    let mut name = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-t" => {
                target = target_value(args.get(i + 1).cloned(), "-t")?;
                i += 2;
            }
            "--" => {
                let remaining = &args[i + 1..];
                if remaining.len() > 1 || (remaining.len() == 1 && name.is_some()) {
                    bail!("tmux rename-session accepts exactly one new session name");
                }
                if let Some(value) = remaining.first() {
                    name = Some(value.clone());
                }
                break;
            }
            flag if flag.starts_with("-t=") => {
                target = target_value(Some(flag[3..].to_string()), "-t")?;
                i += 1;
            }
            flag if flag.starts_with("-t") && flag.len() > 2 => {
                target = target_value(Some(flag[2..].to_string()), "-t")?;
                i += 1;
            }
            flag if flag.starts_with('-') => {
                bail!("unsupported tmux rename-session option: {flag}");
            }
            value => {
                // tmux accepts `rename-session new-name -t target` as well as
                // `rename-session -t target new-name`, so keep parsing after
                // the positional name and reject only a second positional.
                if name.replace(value.to_string()).is_some() {
                    bail!("tmux rename-session accepts exactly one new session name");
                }
                i += 1;
            }
        }
    }
    let name = name.context("tmux rename-session requires a new session name")?;
    let target = target.unwrap_or_else(default_target);
    client::rename_session(&target, &name)?;
    Ok(0)
}

fn split_window(args: &[String]) -> Result<i32> {
    // tmux accepts layout/size/environment options such as `-l 3`
    // before `-d`; these are placement hints for a real tmux pane, but
    // they must still consume their values so detached helper panes stay
    // detached instead of being mistaken for visible commands. `-F`, `-t`,
    // and `-c` are handled explicitly below; `VALUE_FLAGS` covers the
    // split-window-local value flags that the generic parser does not know.
    const VALUE_FLAGS: &[char] = &['e', 'l', 'p'];
    const BOOLEAN_VALUE_OVERRIDES: &[char] = &['f'];
    let mut direction = "right";
    let mut print = false;
    let mut format = "#{pane_id}".to_string();
    let mut target = None;
    let mut cwd = None;
    let mut detached = false;
    let mut command = Vec::new();
    let mut pane_env = HashMap::new();
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
                format = value_for_option(args.get(i + 1).cloned(), "-F")?;
                i += 2;
            }
            "-t" => {
                target = target_value(args.get(i + 1).cloned(), "-t")?;
                i += 2;
            }
            "-c" => {
                cwd = Some(value_for_option(args.get(i + 1).cloned(), "-c")?);
                i += 2;
            }
            "-e" => {
                parse_split_window_env_assignment(
                    value_for_option(args.get(i + 1).cloned(), "-e")?,
                    &mut pane_env,
                )?;
                i += 2;
            }
            "--" => {
                command.extend_from_slice(&args[i + 1..]);
                break;
            }
            flag if flag.starts_with('-') => {
                if has_flag_in_arg_with_value_flags_and_boolean_overrides(
                    flag,
                    'h',
                    VALUE_FLAGS,
                    BOOLEAN_VALUE_OVERRIDES,
                ) {
                    direction = "right";
                }
                if has_flag_in_arg_with_value_flags_and_boolean_overrides(
                    flag,
                    'v',
                    VALUE_FLAGS,
                    BOOLEAN_VALUE_OVERRIDES,
                ) {
                    direction = "down";
                }
                if has_flag_in_arg_with_value_flags_and_boolean_overrides(
                    flag,
                    'd',
                    VALUE_FLAGS,
                    BOOLEAN_VALUE_OVERRIDES,
                ) {
                    detached = true;
                }
                if has_flag_in_arg_with_value_flags_and_boolean_overrides(
                    flag,
                    'P',
                    VALUE_FLAGS,
                    BOOLEAN_VALUE_OVERRIDES,
                ) {
                    print = true;
                }
                if let Some((_, value)) = short_cluster_flag_value_with_extra_and_boolean_overrides(
                    flag,
                    'F',
                    args,
                    i,
                    VALUE_FLAGS,
                    BOOLEAN_VALUE_OVERRIDES,
                ) {
                    format = value_for_option(value.or_else(|| args.get(i + 1).cloned()), "-F")?;
                }
                if let Some((_, value)) = short_cluster_flag_value_with_extra_and_boolean_overrides(
                    flag,
                    't',
                    args,
                    i,
                    VALUE_FLAGS,
                    BOOLEAN_VALUE_OVERRIDES,
                ) {
                    target = target_value(value.or_else(|| args.get(i + 1).cloned()), "-t")?;
                }
                if let Some((_, value)) = short_cluster_flag_value_with_extra_and_boolean_overrides(
                    flag,
                    'c',
                    args,
                    i,
                    VALUE_FLAGS,
                    BOOLEAN_VALUE_OVERRIDES,
                ) {
                    cwd = Some(value_for_option(
                        value.or_else(|| args.get(i + 1).cloned()),
                        "-c",
                    )?);
                }
                if let Some((_, value)) = short_cluster_flag_value_with_extra_and_boolean_overrides(
                    flag,
                    'e',
                    args,
                    i,
                    VALUE_FLAGS,
                    BOOLEAN_VALUE_OVERRIDES,
                ) {
                    parse_split_window_env_assignment(
                        value_for_option(value.or_else(|| args.get(i + 1).cloned()), "-e")?,
                        &mut pane_env,
                    )?;
                }
                i += flag_arg_width_with_extra_and_boolean_overrides(
                    flag,
                    args,
                    i,
                    VALUE_FLAGS,
                    BOOLEAN_VALUE_OVERRIDES,
                );
            }
            _ => {
                command.extend_from_slice(&args[i..]);
                break;
            }
        }
    }
    let command = tmux_shell_command(&command)?;
    if is_omx_hud_watch_command(command.as_deref(), &pane_env) {
        detached = true;
    }

    if let Some(target) = target.as_deref() {
        reject_unsupported_tmux_window_target(target)?;
        if detached {
            ensure_detached_split_target_exists(target)?;
        } else {
            let target = sanitize::terminal_text(target);
            bail!(
                "tmux split-window -t {target} is not supported by lterm compat; \
                 refusing to create a session. Use -d for a detached lterm session \
                 or run `lterm tmux-compat list-commands` for supported commands."
            );
        }
    }

    let cmux_surface = if detached {
        None
    } else {
        open_cmux_split(direction)?
    };
    let mut env = cmux_session_env(cmux_surface.as_ref());
    env.extend(pane_env);
    let info = match client::new_session(None, command, cwd, env, None, true) {
        Ok(info) => info,
        Err(err) => {
            rollback_cmux_split(cmux_surface.as_ref());
            return Err(err);
        }
    };

    if !detached {
        if let Err(err) = send_cmux_attach(cmux_surface.as_ref(), &info) {
            rollback_lterm_pane(&info.pane_id);
            rollback_cmux_split(cmux_surface.as_ref());
            return Err(err);
        }
    }
    if let Err(err) = remember_pane(&info, cmux_surface.as_ref()) {
        rollback_lterm_pane(&info.pane_id);
        rollback_cmux_split(cmux_surface.as_ref());
        return Err(err);
    }

    if print {
        println!("{}", expand_format(&format, &info));
    }
    Ok(0)
}

fn ensure_detached_split_target_exists(target: &str) -> Result<()> {
    let safe_target = sanitize::terminal_text(target);
    // Detached split-window is commonly used by agent HUD/status helpers with
    // `-t <session-name>`. A real tmux accepts an existing live session/window
    // target even when the command is issued from a different current pane. The
    // lterm compat layer creates a separate helper session instead of a visible
    // split inside the requested target pane, so this shim intentionally checks
    // for existence rather than current-pane equality.
    //
    // Security boundary: lterm's daemon socket already enforces same-OS-user
    // peer credentials on every request. Within that same-owner socket boundary,
    // tmux compatibility follows tmux's model that a client able to name a live
    // target can launch a detached helper for orchestration. The helper runs as
    // a separate lterm session and is not attached into the target pane.
    info_for_tmux_target(target)
        .with_context(|| format!("tmux split-window -d target not found: {safe_target}"))?;
    Ok(())
}

fn is_omx_hud_watch_command(command: Option<&str>, pane_env: &HashMap<String, String>) -> bool {
    let Some(command) = command else {
        return false;
    };
    let has_owner_env = pane_env
        .get("OMX_TMUX_HUD_OWNER")
        .is_some_and(|value| value.trim() == "1");
    let has_owner_command = command.contains("OMX_TMUX_HUD_OWNER=1")
        || command.contains("OMX_TMUX_HUD_OWNER='1'")
        || command.contains("OMX_TMUX_HUD_OWNER=\"1\"");
    (has_owner_env || has_owner_command) && command.contains("hud --watch")
}

fn parse_split_window_env_assignment(
    assignment: String,
    env: &mut HashMap<String, String>,
) -> Result<()> {
    let Some((key, value)) = assignment.split_once('=') else {
        bail!("tmux split-window -e requires NAME=value");
    };
    if key.is_empty() {
        bail!("tmux split-window -e requires a non-empty variable name");
    }
    env.insert(key.to_string(), value.to_string());
    Ok(())
}

fn list_panes(args: &[String]) -> Result<i32> {
    reject_filter(args)?;
    let format = parse_format(args).unwrap_or_else(|| "#{pane_id}".to_string());
    if let Some(target) = parse_target(args)? {
        let panes = window_pane_rows_for_target(&target)?;
        for pane in panes {
            println!("{}", expand_format(&format, &pane));
        }
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
                target = target_value(args.get(i + 1).cloned(), "-t")?;
                explicit_target = true;
                i += 2;
            }
            "-F" => {
                message = Some(value_for_option(args.get(i + 1).cloned(), "-F")?);
                i += 2;
            }
            "--" => {
                message = Some(args[i + 1..].join(" "));
                break;
            }
            flag if flag.starts_with('-') => {
                if has_flag_in_arg(flag, 'p') {
                    print = true;
                }
                if let Some((_, value)) = short_cluster_flag_value(flag, 't', args, i) {
                    target = target_value(value.or_else(|| args.get(i + 1).cloned()), "-t")?;
                    explicit_target = true;
                }
                if let Some((_, value)) = short_cluster_flag_value(flag, 'F', args, i) {
                    message = Some(value_for_option(
                        value.or_else(|| args.get(i + 1).cloned()),
                        "-F",
                    )?);
                }
                i += flag_arg_width(flag, args, i);
            }
            _ => {
                message = Some(args[i..].join(" "));
                break;
            }
        }
    }
    let target = target.unwrap_or_else(default_target);
    let info = match info_for_tmux_target(&target) {
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
    let parsed = parse_capture_pane_args(args)?;
    let target = parsed.target.unwrap_or_else(default_target);
    let target = normalize_tmux_target(&target)?;
    let text = client::capture_range(target.as_ref(), parsed.start, parsed.end)?;
    if parsed.print {
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    } else {
        fs::write(paths::buffer_path()?, text)?;
    }
    Ok(0)
}

fn parse_capture_pane_args(args: &[String]) -> Result<CapturePaneArgs> {
    const VALUE_FLAGS: &[char] = &['S', 'E', 'b', 't'];
    let mut parsed = CapturePaneArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                break;
            }
            "-p" => {
                parsed.print = true;
                i += 1;
            }
            "-t" => {
                parsed.target = target_value(args.get(i + 1).cloned(), "-t")?;
                i += 2;
            }
            "-S" => {
                parsed.start = parse_capture_line_value('S', args.get(i + 1).cloned())?;
                i += 2;
            }
            "-E" => {
                parsed.end = parse_capture_line_value('E', args.get(i + 1).cloned())?;
                i += 2;
            }
            "-b" => i += 2,
            "-e" | "-J" => i += 1,
            flag if flag.starts_with('-') => {
                if let Some(value) = flag.strip_prefix("-t=") {
                    parsed.target = target_value(Some(value.to_string()), "-t")?;
                } else if flag.starts_with("-t") && flag.len() > 2 {
                    parsed.target = target_value(Some(flag[2..].to_string()), "-t")?;
                } else if let Some((_, value)) =
                    short_cluster_flag_value_with_extra(flag, 't', args, i, VALUE_FLAGS)
                {
                    parsed.target = match value {
                        Some(value) => target_value(Some(value), "-t")?,
                        None => target_value(args.get(i + 1).cloned(), "-t")?,
                    };
                }
                if has_flag_in_arg_with_value_flags(flag, 'p', VALUE_FLAGS) {
                    parsed.print = true;
                }
                if let Some((_, value)) =
                    short_cluster_flag_value_with_extra(flag, 'S', args, i, VALUE_FLAGS)
                {
                    parsed.start = parse_capture_line_value('S', value)?;
                }
                if let Some((_, value)) =
                    short_cluster_flag_value_with_extra(flag, 'E', args, i, VALUE_FLAGS)
                {
                    parsed.end = parse_capture_line_value('E', value)?;
                }
                i += flag_arg_width_with_extra(flag, args, i, VALUE_FLAGS);
            }
            _ => {
                i += 1;
            }
        }
    }
    Ok(parsed)
}

fn parse_capture_line_value(flag: char, value: Option<String>) -> Result<Option<i32>> {
    let Some(value) = value else {
        bail!("capture-pane -{flag} requires a line value");
    };
    if value == "-" {
        return Ok(None);
    }
    if flag == 'S' && value.eq_ignore_ascii_case("top") {
        return Ok(Some(0));
    }
    value
        .parse::<i32>()
        .map(Some)
        .with_context(|| format!("invalid capture-pane -{flag} line value: {value}"))
}

fn send_keys(args: &[String]) -> Result<i32> {
    const VALUE_FLAGS: &[char] = &['N'];
    let target = parse_target_with_value_flags(args, VALUE_FLAGS)?;
    let mut literal = false;
    let mut repeat = 1usize;
    let mut keys = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-t" => {
                i += 2;
            }
            "-l" => {
                literal = true;
                i += 1;
            }
            "-N" => {
                repeat = parse_send_keys_repeat(args.get(i + 1).cloned())?;
                i += 2;
            }
            "--" => {
                keys.extend_from_slice(&args[i + 1..]);
                break;
            }
            flag if flag.starts_with('-') => {
                if let Some((_, value)) =
                    short_cluster_flag_value_with_extra(flag, 'N', args, i, VALUE_FLAGS)
                {
                    repeat = parse_send_keys_repeat(value.or_else(|| args.get(i + 1).cloned()))?;
                }
                i += flag_arg_width_with_extra(flag, args, i, VALUE_FLAGS)
            }
            _ => {
                keys.extend_from_slice(&args[i..]);
                break;
            }
        }
    }
    let target = target.unwrap_or_else(default_target);
    let target = normalize_tmux_target(&target)?;
    let bytes = keys_to_bytes(&keys, literal);
    let bytes = repeated_send_payload(&bytes, repeat)?;
    client::send(target.as_ref(), bytes)?;
    Ok(0)
}

fn repeated_send_payload(bytes: &[u8], repeat: usize) -> Result<Vec<u8>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let total = bytes
        .len()
        .checked_mul(repeat)
        .context("send-keys -N repeat count is too large")?;
    if total > MAX_SEND_DATA_BYTES {
        bail!("send data exceeds {} bytes", MAX_SEND_DATA_BYTES);
    }
    let mut repeated = Vec::with_capacity(total);
    for _ in 0..repeat {
        repeated.extend_from_slice(bytes);
    }
    Ok(repeated)
}

fn parse_send_keys_repeat(value: Option<String>) -> Result<usize> {
    let value = value.context("send-keys -N requires a repeat count")?;
    let repeat = value
        .parse::<usize>()
        .with_context(|| format!("invalid send-keys -N repeat count: {value}"))?;
    if repeat == 0 {
        bail!("send-keys -N repeat count must be greater than zero");
    }
    Ok(repeat)
}

fn kill_pane(args: &[String]) -> Result<i32> {
    let target = parse_target(args)?.unwrap_or_else(default_target);
    let target = normalize_tmux_target(&target)?;
    kill_pane_with_cmux_cleanup(target.as_ref())?;
    Ok(0)
}

fn resize_pane(args: &[String]) -> Result<i32> {
    let target = parse_target(args)?.unwrap_or_else(default_target);
    let target = normalize_tmux_target(&target)?;
    let mut rows = None;
    let mut cols = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            break;
        }
        if let Some((width, value)) = short_cluster_flag_value(&args[i], 'x', args, i) {
            cols = Some(parse_resize_dimension(
                'x',
                value.or_else(|| args.get(i + 1).cloned()),
            )?);
            i += width_for_value_flag(width, &args[i], args, i, 'x');
            continue;
        }
        if let Some((width, value)) = short_cluster_flag_value(&args[i], 'y', args, i) {
            rows = Some(parse_resize_dimension(
                'y',
                value.or_else(|| args.get(i + 1).cloned()),
            )?);
            i += width_for_value_flag(width, &args[i], args, i, 'y');
            continue;
        }
        if args[i].starts_with('-') {
            i += flag_arg_width(&args[i], args, i);
        } else {
            i += 1;
        }
    }
    // tmux-compat shim 은 attach 가 아닌 컨트롤 채널이므로 subscriber_id = None.
    // server 는 per-client geometry 추적을 거치지 않고 즉시 master.resize 한다
    // (PR #15 legacy 경로).
    match (rows, cols) {
        (Some(rows), Some(cols)) => client::resize(target.as_ref(), rows, cols, None)?,
        (Some(rows), None) => {
            let info = client::info(target.as_ref())?;
            client::resize(target.as_ref(), rows, info.cols, None)?;
        }
        (None, Some(cols)) => {
            let info = client::info(target.as_ref())?;
            client::resize(target.as_ref(), info.rows, cols, None)?;
        }
        (None, None) => {}
    }
    Ok(0)
}

fn set_hook(args: &[String]) -> Result<i32> {
    // Agent runtimes such as OMX install tmux client-resized hooks to keep a
    // HUD pane at a fixed height:
    //
    //   tmux set-hook -t '#{session_id}' 'client-resized[id]' run-shell -b ...
    //
    // lterm has no tmux hook dispatcher yet.  Treat hook set/unset/list forms
    // as accepted compatibility no-ops, while still validating option values
    // that would otherwise swallow the hook name and hide malformed invocations.
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            break;
        }
        if args[i] == "-t" {
            value_for_option(args.get(i + 1).cloned(), "-t")?;
            i += 2;
            continue;
        }
        if let Some(value) = args[i].strip_prefix("-t=") {
            value_for_option(Some(value.to_string()), "-t")?;
            i += 1;
            continue;
        }
        if args[i].starts_with("-t") && args[i].len() > 2 {
            value_for_option(Some(args[i][2..].to_string()), "-t")?;
            i += 1;
            continue;
        }
        if let Some((_, value)) = short_cluster_flag_value(&args[i], 't', args, i) {
            if let Some(value) = value {
                value_for_option(Some(value), "-t")?;
            } else {
                value_for_option(args.get(i + 1).cloned(), "-t")?;
            }
            i += flag_arg_width(&args[i], args, i);
            continue;
        }
        if args[i].starts_with('-') {
            i += flag_arg_width(&args[i], args, i);
        } else {
            break;
        }
    }
    Ok(0)
}

fn width_for_value_flag(pos: usize, arg: &str, args: &[String], i: usize, flag: char) -> usize {
    let Some(cluster) = short_cluster(arg) else {
        return 1;
    };
    let rest = &cluster[pos + flag.len_utf8()..];
    if rest.is_empty() && args.get(i + 1).is_some() {
        2
    } else {
        1
    }
}

fn parse_resize_dimension(flag: char, value: Option<String>) -> Result<u16> {
    let value = value.with_context(|| format!("resize-pane -{flag} requires a dimension value"))?;
    let dimension = value
        .parse::<u16>()
        .with_context(|| format!("invalid resize-pane -{flag} dimension value: {value}"))?;
    if dimension == 0 {
        bail!("resize dimensions must be at least 1");
    }
    Ok(dimension)
}

fn show_option(args: &[String]) -> Result<i32> {
    if show_option_prints_value(args) {
        let option = show_option_name(args);
        let value = tmux_option_value(option.as_deref());
        if show_option_value_only(args) {
            println!("{value}");
        } else if let Some(option) = option {
            println!("{option} {value}");
        } else {
            println!("{value}");
        }
    }
    Ok(0)
}

fn show_option_prints_value(args: &[String]) -> bool {
    has_flag(args, "-g") || has_flag(args, "-v")
}

fn show_option_value_only(args: &[String]) -> bool {
    has_flag(args, "-v")
}

fn show_option_name(args: &[String]) -> Option<String> {
    const VALUE_FLAGS: &[char] = &['t'];
    let mut option = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                option = args.get(i + 1).cloned();
                break;
            }
            flag if flag.starts_with('-') && flag != "-" => {
                i += flag_arg_width_with_extra(flag, args, i, VALUE_FLAGS);
            }
            value => {
                option = Some(value.to_string());
                i += 1;
            }
        }
    }
    option
}

fn tmux_option_value(option: Option<&str>) -> &'static str {
    match option {
        // Claude Code / OMC query this through the tmux shim to decide whether
        // focus tracking and completion notifications are reliable. lterm does
        // not run a real tmux server, but attached PTY streams are raw and focus
        // events can pass through the host terminal/cmux layer, so reporting
        // "on" avoids a misleading "tmux focus-events off" warning.
        Some("focus-events") => "on",
        _ => "off",
    }
}

fn display_popup(args: &[String]) -> Result<i32> {
    const VALUE_FLAGS: &[char] = &['b', 'c', 'd', 'e', 'h', 's', 'S', 't', 'T', 'w', 'x', 'y'];
    let mut command = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                command = tmux_shell_command(&args[i + 1..])?;
                break;
            }
            flag if flag.starts_with('-') && flag != "-" => {
                i += flag_arg_width_with_extra(flag, args, i, VALUE_FLAGS);
            }
            _ => {
                command = tmux_shell_command(&args[i..])?;
                break;
            }
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

fn run_shell(args: &[String]) -> Result<i32> {
    const VALUE_FLAGS: &[char] = &['d', 't'];
    let mut background = false;
    let mut delay = Duration::from_secs(0);
    let mut command = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                command = tmux_shell_command(&args[i + 1..])?;
                break;
            }
            "-b" => {
                background = true;
                i += 1;
            }
            "-d" => {
                delay = parse_run_shell_delay(&value_for_option(
                    args.get(i + 1).cloned(),
                    args[i].as_str(),
                )?)?;
                i += 2;
            }
            "-t" => {
                value_for_option(args.get(i + 1).cloned(), args[i].as_str())?;
                i += 2;
            }
            flag if flag.starts_with('-') && flag != "-" => {
                if has_flag_in_arg_with_value_flags(flag, 'b', VALUE_FLAGS) {
                    background = true;
                }
                if let Some((_, value)) =
                    short_cluster_flag_value_with_extra(flag, 'd', args, i, VALUE_FLAGS)
                {
                    delay = parse_run_shell_delay(&value_for_option(
                        value.or_else(|| args.get(i + 1).cloned()),
                        "-d",
                    )?)?;
                }
                if let Some((_, value)) =
                    short_cluster_flag_value_with_extra(flag, 't', args, i, VALUE_FLAGS)
                {
                    value_for_option(value.or_else(|| args.get(i + 1).cloned()), "-t")?;
                }
                i += flag_arg_width_with_extra(flag, args, i, VALUE_FLAGS);
            }
            _ => {
                command = tmux_shell_command(&args[i..])?;
                break;
            }
        }
    }
    let Some(command) = command else {
        return Ok(0);
    };
    run_shell_command(command, background, delay)
}

fn parse_run_shell_delay(value: &str) -> Result<Duration> {
    let seconds: f64 = value
        .parse()
        .with_context(|| format!("tmux run-shell -d delay must be seconds: {value}"))?;
    Duration::try_from_secs_f64(seconds).map_err(|_| {
        anyhow!("tmux run-shell -d delay must be a finite non-negative duration: {value}")
    })
}

fn run_shell_command(command: String, background: bool, delay: Duration) -> Result<i32> {
    let shell_path = default_shell();
    let mut shell = Command::new(&shell_path);
    if background {
        if delay.is_zero() {
            shell.arg("-c").arg(command);
        } else {
            // Keep the caller-provided delay and command out of the wrapper's
            // shell source so they cannot inject commands before the exec.
            shell
                .arg("-c")
                .arg(
                    "sleep \"$LTERM_RUN_SHELL_DELAY\"; \
                     exec \"$LTERM_RUN_SHELL_SHELL\" -c \"$LTERM_RUN_SHELL_COMMAND\"",
                )
                .env("LTERM_RUN_SHELL_DELAY", delay.as_secs_f64().to_string())
                .env("LTERM_RUN_SHELL_SHELL", &shell_path)
                .env("LTERM_RUN_SHELL_COMMAND", command);
        }
        shell
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("tmux run-shell -b")?;
        Ok(0)
    } else {
        if !delay.is_zero() {
            thread::sleep(delay);
        }
        shell.arg("-c").arg(command);
        let status = shell.status().context("tmux run-shell")?;
        Ok(status.code().unwrap_or(1))
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
                channel = Some(value_for_option(args.get(i + 1).cloned(), "-S")?);
                i += 2;
            }
            "-L" | "-U" => bail!("tmux wait-for {} is not supported by lterm", args[i]),
            other => {
                channel = Some(other.to_string());
                i += 1;
            }
        }
    }
    let channel = channel.context("tmux wait-for requires a channel")?;
    if signal {
        update_store(|store| {
            prune_wait_generations(store, None);
            let generation = store.wait_generations.entry(channel.clone()).or_default();
            *generation = generation.saturating_add(1);
            touch_wait_generation(store, &channel);
            Ok(())
        })?;
        return Ok(0);
    }

    let observed_generation = update_store(|store| {
        prune_wait_generations(store, Some(&channel));
        touch_existing_wait_generation(store, &channel);
        Ok(*store.wait_generations.get(&channel).unwrap_or(&0))
    })?;
    let deadline = Instant::now() + Duration::from_secs(60 * 60 * 24);
    let touch_interval = Duration::from_secs(WAIT_GENERATION_TOUCH_INTERVAL_SECS);
    let mut next_touch = Instant::now() + touch_interval;
    let mut sleep_for = Duration::from_millis(100);
    while Instant::now() < deadline {
        let now = Instant::now();
        let current_generation = if now >= next_touch {
            next_touch = now + touch_interval;
            update_store(|store| {
                prune_wait_generations(store, Some(&channel));
                touch_existing_wait_generation(store, &channel);
                Ok(*store.wait_generations.get(&channel).unwrap_or(&0))
            })?
        } else {
            read_store(|store| Ok(*store.wait_generations.get(&channel).unwrap_or(&0)))?
        };
        if wait_generation_has_advanced(observed_generation, current_generation) {
            return Ok(0);
        }
        thread::sleep(sleep_for);
        sleep_for = (sleep_for + Duration::from_millis(100)).min(Duration::from_secs(1));
    }
    Ok(1)
}

fn load_buffer(args: &[String]) -> Result<i32> {
    let source = buffer_path_arg(args);
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
    let dest = buffer_path_arg(args);
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
    let target = parse_target_with_value_flags(args, &['b', 's'])?.unwrap_or_else(default_target);
    let target = normalize_tmux_target(&target)?;
    let data = read_buffer_or_empty()?;
    client::send(target.as_ref(), data)?;
    Ok(0)
}

fn read_buffer_or_empty() -> Result<Vec<u8>> {
    match fs::read(paths::buffer_path()?) {
        Ok(data) => Ok(data),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).context("read tmux buffer"),
    }
}

fn open_cmux_split(direction: &str) -> Result<Option<CmuxSurfaceContext>> {
    if !inside_cmux() {
        bail!(
            "tmux split-window without -d requires a cmux environment; \
             refusing to create a hidden lterm session. Use -d for a detached session."
        );
    }
    if !client::command_exists("cmux") {
        bail!(
            "tmux split-window without -d requires the cmux CLI in PATH; \
             refusing to create a hidden lterm session. Use -d for a detached session."
        );
    }
    // Prefer cmux's live focused surface over $CMUX_SURFACE_ID: lterm sessions can
    // outlive the parent shell integration env, making those defaults stale.
    let source_surface = cmux_identify_surface()
        .context("cmux identify split source surface")?
        .ok_or_else(|| anyhow!("cmux identify did not report a split source surface id"))?;
    let mut split = Command::new("cmux");
    split.arg("new-split").arg(direction);
    add_cmux_surface_context_args(&mut split, &source_surface);
    split.arg("--focus").arg("true");
    let split_output = run_cmux_command(&mut split).context("cmux new-split")?;
    if !split_output.status.success() {
        let detail = cmux_stderr_suffix(&split_output.stderr.bytes);
        bail!(
            "cmux new-split {direction} failed with {}; \
             refusing to create a hidden lterm session{}",
            split_output.status,
            detail
        );
    }

    if split_output.stdout.truncated {
        rollback_focused_cmux_split(Some(&source_surface));
        bail!(
            "cmux new-split {direction} output exceeded {} bytes; \
             refusing to parse truncated surface metadata",
            CMUX_OUTPUT_CAPTURE_BYTES
        );
    }

    if let Some(mut surface) = parse_cmux_new_split_surface_context(&split_output.stdout.bytes) {
        if surface.workspace_ref.is_none() {
            surface.workspace_ref = source_surface.workspace_ref.clone();
        }
        if surface.window_ref.is_none() {
            surface.window_ref = source_surface.window_ref.clone();
        }
        return Ok(Some(surface));
    }

    let surface = match cmux_identify_surface().context("cmux identify new split surface") {
        Ok(Some(surface)) => surface,
        Ok(None) => {
            rollback_focused_cmux_split(Some(&source_surface));
            bail!("cmux identify did not report a new split surface id");
        }
        Err(err) => {
            rollback_focused_cmux_split(Some(&source_surface));
            return Err(err);
        }
    };
    Ok(Some(surface))
}

fn rollback_lterm_pane(pane_id: &str) {
    if let Err(err) = client::kill(pane_id) {
        eprintln!(
            "warning: lterm pane rollback failed for {}: {}",
            sanitize::terminal_text(pane_id),
            sanitize::terminal_text(&err.to_string())
        );
    }
}

#[cfg(test)]
fn parse_cmux_new_split_surface(output: &[u8]) -> Option<String> {
    parse_cmux_new_split_surface_context(output).map(|surface| surface.surface_ref)
}

fn parse_cmux_new_split_surface_context(output: &[u8]) -> Option<CmuxSurfaceContext> {
    // Current cmux prints `OK surface:<ref> workspace:<ref>` for a successful split.
    // Only trust an explicit OK record; otherwise fall back to `cmux identify`.
    String::from_utf8_lossy(output)
        .lines()
        .rev()
        .find_map(parse_cmux_new_split_surface_line)
}

fn parse_cmux_new_split_surface_line(line: &str) -> Option<CmuxSurfaceContext> {
    let mut tokens = line.split_whitespace();
    let first = trim_cmux_output_token(tokens.next()?);
    if first != "OK" {
        return None;
    }
    let mut surface_ref = None;
    let mut workspace_ref = None;
    let mut window_ref = None;
    for token in tokens.map(trim_cmux_output_token) {
        surface_ref = surface_ref.or_else(|| parse_cmux_surface_ref_token(token));
        workspace_ref = workspace_ref.or_else(|| parse_cmux_workspace_ref_token(token));
        window_ref = window_ref.or_else(|| parse_cmux_window_ref_token(token));
    }
    Some(CmuxSurfaceContext {
        surface_ref: surface_ref?,
        workspace_ref,
        window_ref,
    })
}

fn trim_cmux_output_token(token: &str) -> &str {
    token.trim_matches(|c: char| c == ',' || c == ';')
}

fn parse_cmux_surface_ref_token(token: &str) -> Option<String> {
    parse_cmux_ref_token(token, "surface:")
}

fn parse_cmux_workspace_ref_token(token: &str) -> Option<String> {
    parse_cmux_ref_token(token, "workspace:")
}

fn parse_cmux_window_ref_token(token: &str) -> Option<String> {
    parse_cmux_ref_token(token, "window:")
}

fn parse_cmux_ref_token(token: &str, prefix: &str) -> Option<String> {
    let suffix = token.strip_prefix(prefix)?;
    if !is_valid_cmux_ref_segment(suffix) {
        return None;
    }
    Some(token.to_string())
}

fn is_valid_cmux_ref_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn is_valid_cmux_json_ref(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('-') && value.split(':').all(is_valid_cmux_ref_segment)
}

fn send_cmux_attach(surface: Option<&CmuxSurfaceContext>, info: &SessionInfo) -> Result<()> {
    let lterm = child_lterm_executable();
    let attach_cmd = format!(
        "exec env {}=1 {} attach {}\n",
        MANAGED_ATTACH_ENV,
        quote(&lterm),
        quote(&info.pane_id)
    );
    let mut send = Command::new("cmux");
    if let Some(surface) = surface {
        send.arg("send");
        add_cmux_surface_context_args(&mut send, surface);
        send.arg(&attach_cmd);
    } else {
        send.arg("send").arg(&attach_cmd);
    }
    let send_output = run_cmux_command(&mut send).context("cmux send attach command")?;
    if !send_output.status.success() {
        let detail = cmux_stderr_suffix(&send_output.stderr.bytes);
        bail!(
            "cmux send attach command failed with {}{}",
            send_output.status,
            detail
        );
    }
    Ok(())
}

fn cmux_session_env(surface: Option<&CmuxSurfaceContext>) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Ok(socket_path) = std::env::var("CMUX_SOCKET_PATH") {
        if !socket_path.is_empty() {
            env.insert("CMUX_SOCKET_PATH".to_string(), socket_path);
        }
    }
    let Some(surface) = surface else {
        return env;
    };
    env.insert("CMUX_SURFACE_ID".to_string(), surface.surface_ref.clone());
    if let Some(workspace_ref) = surface.workspace_ref.as_deref() {
        env.insert("CMUX_WORKSPACE_ID".to_string(), workspace_ref.to_string());
    }
    if let Some(window_ref) = surface.window_ref.as_deref() {
        env.insert("CMUX_WINDOW_ID".to_string(), window_ref.to_string());
    }
    env
}

pub enum ManagedAttachDecision {
    Proceed(Option<ManagedAttachGuard>),
    Exit,
}

pub struct ManagedAttachGuard {
    pane_id: String,
    token: String,
    running: Arc<AtomicBool>,
    renewer: Option<thread::JoinHandle<()>>,
}

impl Drop for ManagedAttachGuard {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.renewer.take();
        if let Err(err) = release_managed_attach(&self.pane_id, &self.token) {
            eprintln!(
                "warning: managed cmux attach lease release failed for {}: {}",
                sanitize::terminal_text(&self.pane_id),
                sanitize::terminal_text(&err.to_string())
            );
        }
    }
}

pub fn prepare_managed_attach(target: &str) -> Result<ManagedAttachDecision> {
    if !managed_attach_env_enabled() {
        return Ok(ManagedAttachDecision::Proceed(None));
    }

    let info = client::info(target)?;
    let current_surface = match cmux_identify_managed_attach_surface() {
        Ok(Some(surface)) => surface,
        Ok(None) => {
            eprintln!(
                "warning: managed cmux attach could not identify current surface; \
                 proceeding without duplicate-surface cleanup"
            );
            return Ok(ManagedAttachDecision::Proceed(None));
        }
        Err(err) => {
            eprintln!(
                "warning: managed cmux attach could not identify current surface: {}",
                sanitize::terminal_text(&err.to_string())
            );
            return Ok(ManagedAttachDecision::Proceed(None));
        }
    };
    let token = managed_attach_token();
    let claim = claim_managed_attach(&info.pane_id, &token, &current_surface)?;
    if claim.proceed {
        let running = Arc::new(AtomicBool::new(true));
        let pane_id = info.pane_id;
        let renewer =
            spawn_managed_attach_renewer(pane_id.clone(), token.clone(), Arc::clone(&running));
        return Ok(ManagedAttachDecision::Proceed(Some(ManagedAttachGuard {
            pane_id,
            token,
            running: Arc::clone(&running),
            renewer: Some(renewer),
        })));
    }

    if claim
        .owner_surface_id
        .as_deref()
        .is_some_and(|owner| owner != current_surface.surface_ref)
    {
        close_duplicate_cmux_surface(&current_surface)?;
    }
    Ok(ManagedAttachDecision::Exit)
}

struct ManagedAttachClaim {
    proceed: bool,
    owner_surface_id: Option<String>,
}

fn managed_attach_env_enabled() -> bool {
    std::env::var(MANAGED_ATTACH_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn claim_managed_attach(
    pane_id: &str,
    token: &str,
    current_surface: &CmuxSurfaceContext,
) -> Result<ManagedAttachClaim> {
    update_store(|store| {
        prune_managed_attach_leases(store);
        if let Some(existing) = store.managed_attaches.get(pane_id).cloned() {
            if !lease_owner_blocks_duplicate(&existing) {
                store.managed_attaches.remove(pane_id);
            } else if existing
                .cmux_surface_id
                .as_deref()
                .is_some_and(|owner| owner != current_surface.surface_ref)
            {
                return Ok(ManagedAttachClaim {
                    proceed: false,
                    owner_surface_id: existing.cmux_surface_id.clone(),
                });
            }
        }

        let process_start_id = process_start_identity(std::process::id());
        store.managed_attaches.insert(
            pane_id.to_string(),
            ManagedAttachLease {
                pane_id: pane_id.to_string(),
                token: token.to_string(),
                pid: std::process::id(),
                process_start_id,
                cmux_surface_id: Some(current_surface.surface_ref.clone()),
                cmux_workspace_id: current_surface.workspace_ref.clone(),
                cmux_window_id: current_surface.window_ref.clone(),
                updated_secs: now_unix_secs(),
            },
        );
        Ok(ManagedAttachClaim {
            proceed: true,
            owner_surface_id: Some(current_surface.surface_ref.clone()),
        })
    })
}

fn managed_attach_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{nanos}", std::process::id(), thread_id_token())
}

fn thread_id_token() -> String {
    format!("{:?}", thread::current().id())
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect()
}

fn release_managed_attach(pane_id: &str, token: &str) -> Result<()> {
    update_store(|store| {
        prune_managed_attach_leases(store);
        if store
            .managed_attaches
            .get(pane_id)
            .is_some_and(|lease| lease.token == token)
        {
            store.managed_attaches.remove(pane_id);
        }
        Ok(())
    })
}

fn renew_managed_attach(pane_id: &str, token: &str) -> Result<bool> {
    update_store(|store| {
        prune_managed_attach_leases(store);
        let Some(lease) = store.managed_attaches.get_mut(pane_id) else {
            return Ok(false);
        };
        if lease.token != token {
            return Ok(false);
        }
        lease.updated_secs = now_unix_secs();
        Ok(true)
    })
}

fn spawn_managed_attach_renewer(
    pane_id: String,
    token: String,
    running: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while running.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(MANAGED_ATTACH_RENEW_SECS));
            if !running.load(Ordering::SeqCst) {
                break;
            }
            match renew_managed_attach(&pane_id, &token) {
                Ok(true) => {}
                Ok(false) => break,
                Err(err) => {
                    eprintln!(
                        "warning: managed cmux attach lease renewal failed for {}: {}",
                        sanitize::terminal_text(&pane_id),
                        sanitize::terminal_text(&err.to_string())
                    );
                }
            }
        }
    })
}

fn prune_managed_attach_leases(store: &mut CompatStore) {
    store
        .managed_attaches
        .retain(|_, lease| lease_owner_blocks_duplicate(lease));
}

fn lease_owner_blocks_duplicate(lease: &ManagedAttachLease) -> bool {
    if lease_owner_is_live(lease) {
        return true;
    }
    lease.process_start_id.is_none()
        && process_is_live(lease.pid)
        && lease.updated_secs >= now_unix_secs().saturating_sub(MANAGED_ATTACH_LEASE_TTL_SECS)
}

fn lease_owner_is_live(lease: &ManagedAttachLease) -> bool {
    if !process_is_live(lease.pid) {
        return false;
    }
    let Some(expected) = lease.process_start_id.as_deref() else {
        return false;
    };
    process_start_identity(lease.pid).as_deref() == Some(expected)
}

fn process_is_live(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "macos")]
fn process_start_identity(pid: u32) -> Option<String> {
    let pid = libc::c_int::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let size_i32 = libc::c_int::try_from(size).ok()?;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
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
    Some(format!(
        "macos:{}:{}:{}",
        info.pbi_pid, info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}

#[cfg(target_os = "linux")]
fn process_start_identity(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    let mut fields = after_comm.split_whitespace();
    let start_ticks = fields.nth(19)?;
    Some(format!("linux:{pid}:{start_ticks}"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_start_identity(_pid: u32) -> Option<String> {
    None
}

fn close_duplicate_cmux_surface(surface: &CmuxSurfaceContext) -> Result<()> {
    let mut close = Command::new("cmux");
    close.arg("close-surface");
    add_cmux_surface_context_args(&mut close, surface);
    let close_output =
        run_cmux_command(&mut close).context("cmux managed duplicate close-surface")?;
    if !close_output.status.success() {
        let detail = cmux_stderr_suffix(&close_output.stderr.bytes);
        bail!(
            "cmux managed duplicate close-surface failed with {}{}",
            close_output.status,
            detail
        );
    }
    Ok(())
}

fn child_lterm_executable() -> String {
    if let Ok(value) = std::env::var("LTERM_BIN") {
        if is_safe_lterm_bin_override(&value) {
            return value;
        }
    }
    if let Ok(path) = std::env::current_exe() {
        return path.display().to_string();
    }
    "lterm".to_string()
}

fn is_safe_lterm_bin_override(value: &str) -> bool {
    let path = PathBuf::from(value);
    if !path.is_absolute() || value.chars().any(|ch| ch.is_control()) {
        return false;
    }
    fs::symlink_metadata(path)
        .map(|metadata| {
            metadata.file_type().is_file() && metadata.permissions().mode() & 0o111 != 0
        })
        .unwrap_or(false)
}

fn rollback_cmux_split(surface: Option<&CmuxSurfaceContext>) {
    let Some(surface) = surface else {
        return;
    };
    close_cmux_surface_best_effort("cmux close-surface rollback", surface);
}

fn close_cmux_surface_best_effort(context: &str, surface: &CmuxSurfaceContext) {
    let mut close = Command::new("cmux");
    close.arg("close-surface");
    add_cmux_surface_context_args(&mut close, surface);
    let output = run_cmux_command(&mut close);
    report_cmux_rollback_failure(context, output);
}

fn rollback_focused_cmux_split(source_surface: Option<&CmuxSurfaceContext>) {
    let focused = match cmux_identify_surface() {
        Ok(Some(surface)) => surface,
        Ok(None) => {
            eprintln!(
                "warning: cmux close focused surface rollback skipped: \
                 cmux identify did not report a focused surface id"
            );
            return;
        }
        Err(err) => {
            eprintln!(
                "warning: cmux close focused surface rollback skipped: {}",
                sanitize::terminal_text(&err.to_string())
            );
            return;
        }
    };
    if source_surface.is_some_and(|source| source.surface_ref == focused.surface_ref) {
        eprintln!(
            "warning: cmux close focused surface rollback skipped: \
             focused surface still matches the split source"
        );
        return;
    }
    let mut close = Command::new("cmux");
    close.arg("close-surface");
    add_cmux_surface_context_args(&mut close, &focused);
    let output = run_cmux_command(&mut close);
    report_cmux_rollback_failure("cmux close focused surface rollback", output);
}

fn add_cmux_surface_context_args(command: &mut Command, surface: &CmuxSurfaceContext) {
    command.arg("--surface").arg(&surface.surface_ref);
    if let Some(workspace_ref) = surface.workspace_ref.as_deref() {
        command.arg("--workspace").arg(workspace_ref);
    }
    if let Some(window_ref) = surface.window_ref.as_deref() {
        command.arg("--window").arg(window_ref);
    }
}

fn report_cmux_rollback_failure(context: &str, output: io::Result<CmuxCommandOutput>) {
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => eprintln!(
            "warning: {} failed with {}{}",
            context,
            output.status,
            cmux_stderr_suffix(&output.stderr.bytes)
        ),
        Err(err) => eprintln!(
            "warning: {} failed to run: {}",
            context,
            sanitize::terminal_text(&err.to_string())
        ),
    }
}

fn cmux_stderr_suffix(stderr: &[u8]) -> String {
    cmux_stderr_preview(stderr)
        .map(|detail| format!(": {detail}"))
        .unwrap_or_default()
}

fn cmux_stderr_preview(stderr: &[u8]) -> Option<String> {
    const MAX_CHARS: usize = 512;
    let sanitized = sanitize::terminal_capture(stderr);
    let normalized = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    let mut chars = normalized.chars();
    let mut preview: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    Some(preview)
}

const CMUX_OUTPUT_CAPTURE_BYTES: usize = 16 * 1024;

/// 단일 `cmux` 호출의 최대 대기 시간(설계 §4.4, Codex HIGH Issue 1 대응).
///
/// `run_cmux_command`이 `child.wait()`로 무한 대기하면 cmux가 멈출 때 attach 메인 루프가
/// 프리즈된다. 모든 호출자(open_cmux_split/send_cmux_attach/identify/status sink 등)는 빠른
/// 단발 명령이라 3초면 넉넉하다. 외부 `timeout` 명령은 macOS 비호환이라 순수 Rust 폴링으로
/// 구현한다(아래 [`run_cmux_command`]).
///
/// 주의: status sink의 `apply`는 다수 [`cmux_status::CmuxCommand`]를 순차 실행하므로
/// worst-case stall은 N×이 타임아웃이다. diff 게이트로 호출이 드물고 각 명령이 빨라 실무상
/// 문제되지 않으나, 전면 worker-thread 전환은 본 변경 범위 밖이다.
const CMUX_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

/// [`run_cmux_command`] 타임아웃 폴링 간격. flock 폴링(`StoreLock::acquire`)과 동일한 25ms.
const CMUX_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(25);

struct CmuxCommandOutput {
    status: std::process::ExitStatus,
    stdout: LimitedOutput,
    stderr: LimitedOutput,
}

/// cmux surface 식별 컨텍스트(surface/workspace/window ref). cmux status pill sink가
/// 워크스페이스 타깃을 잡으려면 이 컨텍스트가 필요하므로 `pub(crate)`로 노출한다
/// (`inside_cmux`가 받은 동일 처치, 설계 §4.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CmuxSurfaceContext {
    pub(crate) surface_ref: String,
    pub(crate) workspace_ref: Option<String>,
    pub(crate) window_ref: Option<String>,
}

struct LimitedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn run_cmux_command(command: &mut Command) -> io::Result<CmuxCommandOutput> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("cmux stdout pipe missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("cmux stderr pipe missing"))?;
    let stdout_reader =
        thread::spawn(move || read_limited_output(stdout, CMUX_OUTPUT_CAPTURE_BYTES));
    let stderr_reader =
        thread::spawn(move || read_limited_output(stderr, CMUX_OUTPUT_CAPTURE_BYTES));
    let wait_result = wait_with_timeout(&mut child, CMUX_COMMAND_TIMEOUT);
    // 타임아웃·wait 실패 시 자식을 죽여 reader 스레드의 파이프를 닫는다(블록 해제).
    if wait_result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let stdout = join_limited_reader(stdout_reader)?;
    let stderr = join_limited_reader(stderr_reader)?;
    let status = wait_result?;

    Ok(CmuxCommandOutput {
        status,
        stdout,
        stderr,
    })
}

/// 자식 프로세스를 데드라인까지 `try_wait` 폴링한다(설계 §4.4, Codex HIGH Issue 1).
///
/// `Ok(status)`: 시한 내 종료. `Err(TimedOut)`: 데드라인 초과(호출자가 kill 책임). `Err(_)`:
/// `try_wait` I/O 오류. 외부 `timeout` 명령 대신 순수 Rust 폴링이라 macOS에서도 동작한다.
/// 타임아웃 에러는 status sink 서킷브레이커에 정상 실패로 집계된다(`run` → `Err(_)` → false).
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> io::Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(status),
            None => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "cmux command timed out",
                    ));
                }
                thread::sleep(CMUX_COMMAND_POLL_INTERVAL);
            }
        }
    }
}

fn read_limited_output<R: Read>(mut reader: R, limit: usize) -> io::Result<LimitedOutput> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if read == 0 {
            return Ok(LimitedOutput {
                bytes: output,
                truncated,
            });
        }
        let remaining = limit.saturating_sub(output.len());
        if remaining > 0 {
            output.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        if read > remaining {
            truncated = true;
        }
    }
}

fn join_limited_reader(
    reader: thread::JoinHandle<io::Result<LimitedOutput>>,
) -> io::Result<LimitedOutput> {
    reader
        .join()
        .map_err(|_| io::Error::other("cmux output reader panicked"))?
}

/// cmux 멀티플렉서 내부에서 실행 중인지 판정한다(`CMUX_*` env / 검증된 소켓).
///
/// status 백엔드 라우팅([`crate::client`]의 `gather_status_env_snapshot`)이 `inside_cmux` 신호를
/// 읽을 수 있도록 `pub(crate)`로 노출한다(이전엔 module-private였음).
pub(crate) fn inside_cmux() -> bool {
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

fn cmux_identify_surface() -> Result<Option<CmuxSurfaceContext>> {
    cmux_identify_surface_with(find_cmux_surface_context, None)
}

fn cmux_identify_managed_attach_surface() -> Result<Option<CmuxSurfaceContext>> {
    cmux_identify_surface_with(find_cmux_managed_attach_surface_context, None)
}

/// `cmux identify` 인자 목록을 구성한다(순수 함수, side-effect 없어 테스트 가능).
///
/// 항상 `identify --json`을 내고, `id_format`이 `Some(fmt)`면 `--id-format <fmt>`를 덧붙인다.
fn cmux_identify_args(id_format: Option<&str>) -> Vec<String> {
    let mut args = vec!["identify".to_string(), "--json".to_string()];
    if let Some(format) = id_format {
        args.push("--id-format".to_string());
        args.push(format.to_string());
    }
    args
}

/// `cmux identify --json [--id-format <fmt>]`을 실행해 `select_surface`로 컨텍스트를 고른다.
///
/// `id_format`이 `Some("uuids")`면 positional ref 재번호화 드리프트(R1)를 피하기 위해
/// UUID 식별자를 요청한다(설계 §4.2/ADR-6). `None`이면 기존 동작(기본 ref 포맷)을 유지한다.
fn cmux_identify_surface_with(
    select_surface: fn(&serde_json::Value) -> Option<CmuxSurfaceContext>,
    id_format: Option<&str>,
) -> Result<Option<CmuxSurfaceContext>> {
    let mut identify = Command::new("cmux");
    identify.args(cmux_identify_args(id_format));
    let output = run_cmux_command(&mut identify)?;
    if !output.status.success() {
        return Ok(None);
    }
    if output.stdout.truncated {
        bail!(
            "cmux identify --json output exceeded {} bytes; refusing to parse truncated surface metadata",
            CMUX_OUTPUT_CAPTURE_BYTES
        );
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout.bytes)?;
    Ok(select_surface(&value))
}

#[cfg(test)]
fn find_cmux_surface_ref(value: &serde_json::Value) -> Option<String> {
    find_cmux_surface_context(value).map(|surface| surface.surface_ref)
}

#[cfg(test)]
fn find_cmux_managed_attach_surface_ref(value: &serde_json::Value) -> Option<String> {
    find_cmux_managed_attach_surface_context(value).map(|surface| surface.surface_ref)
}

fn find_cmux_surface_context(value: &serde_json::Value) -> Option<CmuxSurfaceContext> {
    // Modern cmux identify payloads include caller/focused surfaces. Once that
    // schema is present, trust focused exclusively. If focused is unavailable,
    // fail rather than reusing caller/env metadata that may point at a stale
    // surface or be mistaken for a newly-created split.
    if let Some(focused) = value.get("focused") {
        return cmux_surface_context_from_json(focused);
    }
    if value.get("caller").is_some() {
        return None;
    }
    cmux_surface_context_from_json(value)
}

fn find_cmux_managed_attach_surface_context(
    value: &serde_json::Value,
) -> Option<CmuxSurfaceContext> {
    // Duplicate attach cleanup must close the surface executing this attach,
    // not whichever surface is currently focused. Modern cmux identify payloads
    // carry that as caller/current metadata. Try only those explicit executing
    // surface identities, skipping malformed candidates; if all are missing or
    // malformed, fall back to a normal attach without cleanup instead of
    // risking a close against an unrelated focused surface.
    for key in ["caller", "current", "executing"] {
        if let Some(surface) = value.get(key) {
            if let Some(context) = cmux_surface_context_from_json(surface) {
                return Some(context);
            }
        }
    }
    None
}

fn cmux_surface_context_from_json(value: &serde_json::Value) -> Option<CmuxSurfaceContext> {
    let surface_keys = [
        "surface_id",
        "surfaceId",
        "surface_ref",
        "surfaceRef",
        "surface",
        "id",
    ];
    let workspace_keys = [
        "workspace_id",
        "workspaceId",
        "workspace_ref",
        "workspaceRef",
        "workspace",
    ];
    let window_keys = ["window_id", "windowId", "window_ref", "windowRef", "window"];
    Some(CmuxSurfaceContext {
        surface_ref: find_json_cmux_ref(value, &surface_keys)?,
        workspace_ref: find_json_cmux_ref(value, &workspace_keys),
        window_ref: find_json_cmux_ref(value, &window_keys),
    })
}

fn find_json_cmux_ref(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    find_json_string(value, keys).filter(|value| is_valid_cmux_json_ref(value))
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

fn remember_pane(info: &SessionInfo, cmux_surface: Option<&CmuxSurfaceContext>) -> Result<()> {
    update_store(|store| {
        store.panes.insert(
            info.pane_id.clone(),
            CompatPane {
                pane_id: info.pane_id.clone(),
                session_name: info.name.clone(),
                cmux_surface_id: cmux_surface.map(|surface| surface.surface_ref.clone()),
                cmux_workspace_id: cmux_surface.and_then(|surface| surface.workspace_ref.clone()),
                cmux_window_id: cmux_surface.and_then(|surface| surface.window_ref.clone()),
            },
        );
        Ok(())
    })
}

fn stored_cmux_surface_for_pane(pane_id: &str) -> Result<Option<CmuxSurfaceContext>> {
    read_store(|store| {
        Ok(store.panes.get(pane_id).and_then(|pane| {
            Some(CmuxSurfaceContext {
                surface_ref: pane.cmux_surface_id.clone()?,
                workspace_ref: pane.cmux_workspace_id.clone(),
                window_ref: pane.cmux_window_id.clone(),
            })
        }))
    })
}

fn stored_cmux_surface_for_pane_best_effort(pane_id: &str) -> Option<CmuxSurfaceContext> {
    match stored_cmux_surface_for_pane(pane_id) {
        Ok(surface) => surface,
        Err(err) => {
            eprintln!(
                "warning: tmux compat store lookup failed for {}: {}",
                sanitize::terminal_text(pane_id),
                sanitize::terminal_text(&err.to_string())
            );
            None
        }
    }
}

/// cmux status pill sink가 쓸 워크스페이스 식별 컨텍스트를 attach 시점 1회 확보한다(설계 §4.2).
///
/// 우선순위:
/// 1. **stored split-time 컨텍스트**(`stored_cmux_surface_for_pane`) — split 생성 시점에
///    포커스 결정적으로 캡처된 값이라 최우선. 있으면 `cmux identify`를 호출하지 않는다.
/// 2. 없으면 **`cmux identify --json --id-format uuids`의 focused** — attach 순간엔 대상
///    서피스가 포커스이므로 정당하다. `find_cmux_surface_context`의 stale-`caller` 거부
///    로직을 그대로 재사용한다(caller만 있고 focused가 없으면 `None`).
///
/// UUID 우선 캡처(`--id-format uuids`)로 positional ref 재번호화 드리프트(R1)를 피한다.
/// env `$CMUX_WORKSPACE_ID`는 stale 위험(R2) 때문에 타깃 식별에 절대 쓰지 않는다.
///
/// 반환값: 식별된 컨텍스트, 또는 stored도 identify도 실패하면 `None`(best-effort —
/// 식별 실패가 호출부를 막지 않게 한다. 실제 sink 미생성/blackout 처리는 후속 청크).
pub(crate) fn cmux_status_identity(pane_id: &str) -> Option<CmuxSurfaceContext> {
    // 1. stored split-time 컨텍스트 우선(store 에러는 best-effort로 무시).
    if let Some(stored) = stored_cmux_surface_for_pane_best_effort(pane_id) {
        return Some(stored);
    }
    // 2. identify 폴백(UUID 우선). identify 실패/비-cmux/파싱 실패는 None으로 안전 저하.
    match cmux_identify_surface_with(find_cmux_surface_context, Some("uuids")) {
        Ok(surface) => surface,
        Err(err) => {
            eprintln!(
                "warning: cmux identify for status pill target failed: {}",
                sanitize::terminal_text(&err.to_string())
            );
            None
        }
    }
}

fn forget_pane(pane_id: &str) -> Result<()> {
    update_store(|store| {
        store.panes.remove(pane_id);
        Ok(())
    })
}

fn forget_pane_best_effort(pane_id: &str) {
    if let Err(err) = forget_pane(pane_id) {
        eprintln!(
            "warning: tmux compat store cleanup failed for {}: {}",
            sanitize::terminal_text(pane_id),
            sanitize::terminal_text(&err.to_string())
        );
    }
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn touch_wait_generation(store: &mut CompatStore, channel: &str) {
    store
        .wait_generation_touched_secs
        .insert(channel.to_string(), now_unix_secs());
}

fn touch_existing_wait_generation(store: &mut CompatStore, channel: &str) {
    if store.wait_generations.contains_key(channel) {
        touch_wait_generation(store, channel);
    }
}

fn wait_generation_has_advanced(observed_generation: u64, current_generation: u64) -> bool {
    current_generation > observed_generation
        || (observed_generation > 0
            && current_generation > 0
            && current_generation < observed_generation)
}

fn prune_wait_generations(store: &mut CompatStore, protected_channel: Option<&str>) {
    let now = now_unix_secs();
    let cutoff = now.saturating_sub(WAIT_GENERATION_RETENTION_SECS);
    for channel in store.wait_generations.keys() {
        store
            .wait_generation_touched_secs
            .entry(channel.clone())
            .or_insert(now);
    }
    let stale: Vec<String> = store
        .wait_generation_touched_secs
        .iter()
        .filter_map(|(channel, touched)| {
            if protected_channel == Some(channel.as_str()) {
                return None;
            }
            (*touched < cutoff).then(|| channel.clone())
        })
        .collect();
    for channel in stale {
        store.wait_generation_touched_secs.remove(&channel);
        store.wait_generations.remove(&channel);
    }
    let orphaned_touched: Vec<String> = store
        .wait_generation_touched_secs
        .keys()
        .filter(|channel| !store.wait_generations.contains_key(*channel))
        .cloned()
        .collect();
    for channel in orphaned_touched {
        store.wait_generation_touched_secs.remove(&channel);
    }
    if store.wait_generations.len() <= WAIT_GENERATION_MAX_CHANNELS {
        return;
    }
    let mut channels: Vec<(String, u64)> = store
        .wait_generations
        .keys()
        .filter(|channel| protected_channel != Some(channel.as_str()))
        .map(|channel| {
            (
                channel.clone(),
                *store
                    .wait_generation_touched_secs
                    .get(channel)
                    .unwrap_or(&now),
            )
        })
        .collect();
    channels.sort_by_key(|(_, touched)| *touched);
    let overflow = store
        .wait_generations
        .len()
        .saturating_sub(WAIT_GENERATION_MAX_CHANNELS);
    for (channel, _) in channels.into_iter().take(overflow) {
        store.wait_generation_touched_secs.remove(&channel);
        store.wait_generations.remove(&channel);
    }
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

fn read_store<T>(f: impl FnOnce(&CompatStore) -> Result<T>) -> Result<T> {
    let _lock = StoreLock::acquire()?;
    let store = load_store()?;
    f(&store)
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
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
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
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn parse_target(args: &[String]) -> Result<Option<String>> {
    parse_target_with_value_flags(args, &[])
}

fn parse_target_with_value_flags(
    args: &[String],
    extra_value_flags: &[char],
) -> Result<Option<String>> {
    let mut target = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            break;
        }
        if args[i] == "-t" {
            target = target_value(args.get(i + 1).cloned(), "-t")?;
            i += 2;
            continue;
        }
        if let Some(value) = args[i].strip_prefix("-t=") {
            target = target_value(Some(value.to_string()), "-t")?;
            i += 1;
            continue;
        }
        if args[i].starts_with("-t") && args[i].len() > 2 {
            target = target_value(Some(args[i][2..].to_string()), "-t")?;
            i += 1;
            continue;
        }
        if let Some((_, value)) =
            short_cluster_flag_value_with_extra(&args[i], 't', args, i, extra_value_flags)
        {
            target = if let Some(value) = value {
                target_value(Some(value), "-t")?
            } else {
                target_value(args.get(i + 1).cloned(), "-t")?
            };
            i += flag_arg_width_with_extra(&args[i], args, i, extra_value_flags);
            continue;
        }
        if args[i].starts_with('-') {
            i += flag_arg_width_with_extra(&args[i], args, i, extra_value_flags);
        } else {
            break;
        }
    }
    Ok(target)
}

fn target_value(value: Option<String>, flag: &str) -> Result<Option<String>> {
    match value {
        Some(value) if !value.is_empty() => Ok(Some(value)),
        _ => bail!("tmux target flag {flag} requires a value"),
    }
}

fn tmux_window_target_session(target: &str) -> Option<&str> {
    let (session, window) = target.split_once(':')?;
    if session.is_empty() {
        return None;
    }
    // lterm has a single tmux-compatible window per session. Only the exact
    // `session:0` window form falls back to the root session; pane suffixes and
    // unsupported window indexes must preserve the original lookup failure.
    if window != "0" {
        return None;
    }
    Some(session)
}

fn reject_unsupported_tmux_window_target(target: &str) -> Result<()> {
    let Some((session, window)) = target.split_once(':') else {
        return Ok(());
    };
    if !session.is_empty() && window == "0" {
        return Ok(());
    }
    let target = sanitize::terminal_text(target);
    bail!(
        "unsupported tmux window target in lterm compat: {target}. \
         lterm supports bare session targets and session:0 only."
    )
}

fn normalize_tmux_target(target: &str) -> Result<Cow<'_, str>> {
    reject_unsupported_tmux_window_target(target)?;
    if let Some(session) = tmux_window_target_session(target) {
        Ok(Cow::Borrowed(session))
    } else {
        Ok(Cow::Borrowed(target))
    }
}

fn info_for_tmux_target(target: &str) -> Result<SessionInfo> {
    reject_unsupported_tmux_window_target(target)?;
    match client::info(target) {
        Ok(info) => Ok(info),
        Err(err) => {
            let Some(session) = tmux_window_target_session(target) else {
                return Err(err);
            };
            client::info(session).with_context(|| {
                format!(
                    "tmux target {} resolved to session {}",
                    sanitize::terminal_text(target),
                    sanitize::terminal_text(session)
                )
            })
        }
    }
}

fn value_for_option(value: Option<String>, flag: &str) -> Result<String> {
    value.with_context(|| format!("tmux option {flag} requires a value"))
}

fn has_flag_in_arg(arg: &str, needle: char) -> bool {
    has_flag_in_arg_with_value_flags(arg, needle, &[])
}

fn has_flag_in_arg_with_value_flags(arg: &str, needle: char, extra_value_flags: &[char]) -> bool {
    has_flag_in_arg_with_value_flags_and_boolean_overrides(arg, needle, extra_value_flags, &[])
}

fn has_flag_in_arg_with_value_flags_and_boolean_overrides(
    arg: &str,
    needle: char,
    extra_value_flags: &[char],
    boolean_value_overrides: &[char],
) -> bool {
    let Some(cluster) = short_cluster(arg) else {
        return false;
    };
    for (pos, flag) in cluster.char_indices() {
        if flag == needle {
            return true;
        }
        if value_for_short_flag_with_extra_and_boolean_overrides(
            cluster,
            pos,
            flag,
            &[],
            0,
            extra_value_flags,
            boolean_value_overrides,
        )
        .is_some()
            || (short_flag_takes_value(flag, extra_value_flags, boolean_value_overrides)
                && cluster[pos + flag.len_utf8()..].is_empty())
        {
            break;
        }
    }
    false
}

fn buffer_path_arg(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            return args.get(i + 1).cloned();
        }
        if let Some(width) = buffer_option_width(&args[i], args, i) {
            i += width;
            continue;
        }
        if args[i].starts_with('-') {
            i += flag_arg_width(&args[i], args, i);
        } else {
            return Some(args[i].clone());
        }
    }
    None
}

fn buffer_option_width(arg: &str, args: &[String], i: usize) -> Option<usize> {
    if arg == "-b" {
        return Some(if args.get(i + 1).is_some() { 2 } else { 1 });
    }
    if arg.strip_prefix("-b=").is_some() || (arg.starts_with("-b") && arg.len() > 2) {
        return Some(1);
    }
    let cluster = short_cluster(arg)?;
    for (pos, flag) in cluster.char_indices() {
        if flag == 'b' {
            let rest = &cluster[pos + flag.len_utf8()..];
            return Some(if rest.is_empty() && args.get(i + 1).is_some() {
                2
            } else {
                1
            });
        }
        if value_for_short_flag(cluster, pos, flag, args, i).is_some()
            || (is_value_taking_short_flag(flag) && cluster[pos + flag.len_utf8()..].is_empty())
        {
            break;
        }
    }
    None
}

fn parse_format(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            break;
        }
        if args[i] == "-F" {
            return args.get(i + 1).cloned();
        }
        if let Some(value) = args[i].strip_prefix("-F=") {
            return Some(value.to_string());
        }
        if args[i].starts_with("-F") && args[i].len() > 2 {
            let value = &args[i][2..];
            return Some(value.strip_prefix('=').unwrap_or(value).to_string());
        }
        if let Some((_, value)) = short_cluster_flag_value(&args[i], 'F', args, i) {
            if let Some(value) = value {
                return Some(value);
            }
            return args.get(i + 1).cloned();
        }
        if args[i].starts_with('-') {
            i += flag_arg_width(&args[i], args, i);
        } else {
            i += 1;
        }
    }
    None
}

fn parse_command_filter(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            return args.get(i + 1).cloned();
        }
        if args[i] == "-F" {
            i += 2;
        } else if args[i].starts_with("-F") {
            i += 1;
        } else if args[i].starts_with('-') {
            i += flag_arg_width(&args[i], args, i);
        } else {
            return Some(args[i].clone());
        }
    }
    None
}

fn reject_filter(args: &[String]) -> Result<()> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            break;
        }
        if args[i] == "-f" || short_cluster_flag_value(&args[i], 'f', args, i).is_some() {
            bail!("tmux -f filters are not supported by lterm compat");
        }
        if args[i].starts_with('-') {
            i += flag_arg_width(&args[i], args, i);
        } else {
            i += 1;
        }
    }
    Ok(())
}

fn has_flag(args: &[String], needle: &str) -> bool {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            return false;
        }
        if args[i] == needle {
            return true;
        }
        if let Some(short) = needle.strip_prefix('-') {
            if short.len() == 1 {
                if let Some(cluster) = args[i].strip_prefix('-') {
                    if !cluster.starts_with('-')
                        && short_cluster_flag_value(&args[i], needle_char(short), args, i).is_some()
                    {
                        return true;
                    }
                }
            }
        }
        if args[i].starts_with('-') {
            i += flag_arg_width(&args[i], args, i);
        } else {
            i += 1;
        }
    }
    false
}

fn needle_char(short: &str) -> char {
    short
        .chars()
        .next()
        .expect("short option needle should contain one char")
}

fn flag_arg_width(flag: &str, args: &[String], i: usize) -> usize {
    flag_arg_width_with_extra(flag, args, i, &[])
}

fn flag_arg_width_with_extra(
    flag: &str,
    args: &[String],
    i: usize,
    extra_value_flags: &[char],
) -> usize {
    flag_arg_width_with_extra_and_boolean_overrides(flag, args, i, extra_value_flags, &[])
}

fn flag_arg_width_with_extra_and_boolean_overrides(
    flag: &str,
    args: &[String],
    i: usize,
    extra_value_flags: &[char],
    boolean_value_overrides: &[char],
) -> usize {
    if let Some(cluster) = short_cluster(flag) {
        for (pos, short_flag) in cluster.char_indices() {
            if short_flag_takes_value(short_flag, extra_value_flags, boolean_value_overrides) {
                let rest = &cluster[pos + short_flag.len_utf8()..];
                return if rest.is_empty() && args.get(i + 1).is_some() {
                    2
                } else {
                    1
                };
            }
        }
    }
    1
}

fn short_cluster_flag_value(
    arg: &str,
    needle: char,
    args: &[String],
    i: usize,
) -> Option<(usize, Option<String>)> {
    short_cluster_flag_value_with_extra(arg, needle, args, i, &[])
}

fn short_cluster_flag_value_with_extra(
    arg: &str,
    needle: char,
    args: &[String],
    i: usize,
    extra_value_flags: &[char],
) -> Option<(usize, Option<String>)> {
    short_cluster_flag_value_with_extra_and_boolean_overrides(
        arg,
        needle,
        args,
        i,
        extra_value_flags,
        &[],
    )
}

fn short_cluster_flag_value_with_extra_and_boolean_overrides(
    arg: &str,
    needle: char,
    args: &[String],
    i: usize,
    extra_value_flags: &[char],
    boolean_value_overrides: &[char],
) -> Option<(usize, Option<String>)> {
    let cluster = short_cluster(arg)?;
    for (pos, flag) in cluster.char_indices() {
        let value = value_for_short_flag_with_extra_and_boolean_overrides(
            cluster,
            pos,
            flag,
            args,
            i,
            extra_value_flags,
            boolean_value_overrides,
        );
        if flag == needle {
            return Some((pos, value));
        }
        if value.is_some()
            || (short_flag_takes_value(flag, extra_value_flags, boolean_value_overrides)
                && cluster[pos + flag.len_utf8()..].is_empty())
        {
            break;
        }
    }
    None
}

fn value_for_short_flag(
    cluster: &str,
    pos: usize,
    flag: char,
    args: &[String],
    i: usize,
) -> Option<String> {
    value_for_short_flag_with_extra(cluster, pos, flag, args, i, &[])
}

fn value_for_short_flag_with_extra(
    cluster: &str,
    pos: usize,
    flag: char,
    args: &[String],
    i: usize,
    extra_value_flags: &[char],
) -> Option<String> {
    value_for_short_flag_with_extra_and_boolean_overrides(
        cluster,
        pos,
        flag,
        args,
        i,
        extra_value_flags,
        &[],
    )
}

fn value_for_short_flag_with_extra_and_boolean_overrides(
    cluster: &str,
    pos: usize,
    flag: char,
    args: &[String],
    i: usize,
    extra_value_flags: &[char],
    boolean_value_overrides: &[char],
) -> Option<String> {
    if !short_flag_takes_value(flag, extra_value_flags, boolean_value_overrides) {
        return None;
    }
    let rest = &cluster[pos + flag.len_utf8()..];
    if rest.is_empty() {
        return args.get(i + 1).cloned();
    }
    Some(rest.strip_prefix('=').unwrap_or(rest).to_string())
}

fn short_flag_takes_value(
    flag: char,
    extra_value_flags: &[char],
    boolean_value_overrides: &[char],
) -> bool {
    (is_value_taking_short_flag(flag) && !boolean_value_overrides.contains(&flag))
        || extra_value_flags.contains(&flag)
}

fn short_cluster(arg: &str) -> Option<&str> {
    let cluster = arg.strip_prefix('-')?;
    if cluster.is_empty() || cluster.starts_with('-') {
        None
    } else {
        Some(cluster)
    }
}

fn is_value_taking_short_flag(flag: char) -> bool {
    matches!(flag, 'F' | 't' | 'f' | 'c' | 'n' | 'x' | 'y')
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

fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
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
            out.push_str(&sanitize::terminal_text(value.as_ref()));
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
) -> Option<(&'static str, Cow<'a, str>)> {
    const ACTIVE: &str = "1";
    const CLIENT_NAME: &str = "lterm";
    const CLIENT_PID: &str = "";
    const CLIENT_TTY: &str = "";
    const EXTENDED_KEYS_FORMAT: &str = "xterm";
    const IN_MODE: &str = "0";
    const WINDOW_INDEX: &str = "0";
    const WINDOW_PANES: &str = "1";
    if rest.starts_with("#{pane_id}") {
        Some(("#{pane_id}", Cow::Borrowed(info.pane_id.as_str())))
    } else if rest.starts_with("#D") {
        Some(("#D", Cow::Borrowed(info.pane_id.as_str())))
    } else if rest.starts_with("#{session_name}") {
        Some(("#{session_name}", Cow::Borrowed(info.name.as_str())))
    } else if rest.starts_with("#S") {
        Some(("#S", Cow::Borrowed(info.name.as_str())))
    } else if rest.starts_with("#I") {
        Some(("#I", Cow::Borrowed(WINDOW_INDEX)))
    } else if rest.starts_with("#{pane_current_command}") {
        Some(("#{pane_current_command}", Cow::Borrowed(current_command)))
    } else if rest.starts_with("#{pane_start_command}") {
        Some((
            "#{pane_start_command}",
            Cow::Borrowed(info.command.as_str()),
        ))
    } else if rest.starts_with("#{pane_current_path}") {
        Some(("#{pane_current_path}", Cow::Borrowed(info.cwd.as_str())))
    } else if rest.starts_with("#{pane_active}") {
        Some(("#{pane_active}", Cow::Borrowed(ACTIVE)))
    } else if rest.starts_with("#{pane_dead}") {
        Some((
            "#{pane_dead}",
            Cow::Borrowed(if info.alive { "0" } else { "1" }),
        ))
    } else if rest.starts_with("#{pane_pid}") {
        Some((
            "#{pane_pid}",
            Cow::Owned(
                info.process_id
                    .map(|pid| pid.to_string())
                    .unwrap_or_default(),
            ),
        ))
    } else if rest.starts_with("#{pane_in_mode}") {
        Some(("#{pane_in_mode}", Cow::Borrowed(IN_MODE)))
    } else if rest.starts_with("#{window_id}") {
        Some((
            "#{window_id}",
            Cow::Owned(format!("@{}", info.pane_id.trim_start_matches('%'))),
        ))
    } else if rest.starts_with("#{window_index}") {
        Some(("#{window_index}", Cow::Borrowed(WINDOW_INDEX)))
    } else if rest.starts_with("#{window_name}") {
        Some(("#{window_name}", Cow::Borrowed(info.name.as_str())))
    } else if rest.starts_with("#{window_active}") {
        Some(("#{window_active}", Cow::Borrowed(ACTIVE)))
    } else if rest.starts_with("#{window_panes}") {
        Some(("#{window_panes}", Cow::Borrowed(WINDOW_PANES)))
    } else if rest.starts_with("#{client_name}") {
        Some(("#{client_name}", Cow::Borrowed(CLIENT_NAME)))
    } else if rest.starts_with("#{client_session}") {
        Some(("#{client_session}", Cow::Borrowed(info.name.as_str())))
    } else if rest.starts_with("#{client_pane}") {
        Some(("#{client_pane}", Cow::Borrowed(info.pane_id.as_str())))
    } else if rest.starts_with("#{client_pid}") {
        Some(("#{client_pid}", Cow::Borrowed(CLIENT_PID)))
    } else if rest.starts_with("#{client_tty}") {
        Some(("#{client_tty}", Cow::Borrowed(CLIENT_TTY)))
    } else if rest.starts_with("#{extended-keys-format}") {
        Some((
            "#{extended-keys-format}",
            Cow::Borrowed(EXTENDED_KEYS_FORMAT),
        ))
    } else if rest.starts_with("#{window_width}") {
        Some(("#{window_width}", Cow::Owned(info.cols.to_string())))
    } else if rest.starts_with("#{window_height}") {
        Some(("#{window_height}", Cow::Owned(info.rows.to_string())))
    } else {
        None
    }
}

fn expand_command_format(format: &str, command: &str, alias: Option<&str>) -> String {
    let alias = alias.unwrap_or_default();
    let usage = sanitize::terminal_text(command_usage(command));
    let command = sanitize::terminal_text(command);
    let alias = sanitize::terminal_text(alias);
    format
        .replace("#{command_list_name}", &command)
        .replace("#{command_list_alias}", &alias)
        .replace("#{command_list_usage}", &usage)
        .replace("#{command_name}", &command)
        .replace("#{command_alias}", &alias)
}

fn command_support_tier(command: &str) -> &'static str {
    match command {
        "refresh-client" | "select-layout" | "select-pane" | "set-environment" | "set-option"
        | "set-hook" | "set-window-option" | "show-environment" => "noop",
        "attach-session" | "capture-pane" | "has-session" | "kill-pane" | "kill-session"
        | "list-commands" | "list-sessions" | "rename-session" | "run-shell" | "send-keys" => {
            "full"
        }
        _ => "partial",
    }
}

fn debug_unsupported_command(command: &str, args: &[String]) {
    if !std::env::var("LTERM_DEBUG_TMUX").is_ok_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    }) {
        return;
    }
    eprintln!(
        "lterm_tmux_compat\tunsupported_command\t{}\t{}",
        sanitize::terminal_text(command),
        sanitize::terminal_text(&args.join(" "))
    );
}

const SUPPORTED_COMMANDS: &[(&str, Option<&str>, &[&str])] = &[
    ("attach-session", Some("attach"), &["a"]),
    ("capture-pane", Some("capturep"), &[]),
    ("display-message", Some("display"), &[]),
    ("display-popup", Some("popup"), &[]),
    ("has-session", Some("has"), &[]),
    ("kill-pane", Some("killp"), &[]),
    ("kill-session", None, &[]),
    ("list-clients", Some("lsc"), &[]),
    ("list-commands", Some("lscm"), &[]),
    ("list-panes", Some("lsp"), &[]),
    ("list-sessions", Some("ls"), &[]),
    ("list-windows", Some("lsw"), &[]),
    ("load-buffer", Some("loadb"), &[]),
    ("new-session", Some("new"), &[]),
    ("paste-buffer", Some("pasteb"), &[]),
    ("refresh-client", Some("refresh"), &[]),
    ("rename-session", Some("rename"), &[]),
    ("resize-pane", Some("resizep"), &[]),
    ("run-shell", Some("run"), &[]),
    ("save-buffer", Some("saveb"), &[]),
    ("select-layout", Some("selectl"), &[]),
    ("select-pane", Some("selectp"), &[]),
    ("send-keys", Some("send"), &[]),
    ("set-environment", Some("setenv"), &[]),
    ("set-hook", Some("seth"), &[]),
    ("set-option", Some("set"), &[]),
    ("set-window-option", Some("setw"), &[]),
    ("show-environment", Some("showenv"), &[]),
    ("show-options", Some("show"), &["show-option"]),
    (
        "show-window-options",
        Some("showw"),
        &["show-window-option"],
    ),
    ("split-window", Some("splitw"), &[]),
    ("wait-for", Some("wait"), &[]),
];

fn command_usage(command: &str) -> &'static str {
    match command {
        "attach-session" => "[-dErx] [-c working-directory] [-f flags] [-t target-session]",
        "capture-pane" => "[-aCeJMNpPqT] [-E end-line] [-S start-line] [-t target-pane]",
        "display-message" => "[-p] [-F format] [-t target-pane] [message]",
        "display-popup" => "[-E] [shell-command [argument ...]]",
        "has-session" => "[-t target-session]",
        "kill-pane" => "[-t target-pane]",
        "kill-session" => "[-t target-session]",
        "list-clients" => "[-F format] [-t target-session]",
        "list-commands" => "[-F format] [command]",
        "list-panes" => "[-F format] [-t target-pane]",
        "list-sessions" => "[-F format]",
        "list-windows" => "[-a] [-F format] [-t target-session]",
        "load-buffer" => "path",
        "new-session" => "[-d] [-c start-directory] [-s session-name] [shell-command]",
        "paste-buffer" => "[-t target-pane]",
        "refresh-client" => "[-S] [-t target-client]",
        "rename-session" => "[-t target-session] new-name",
        "resize-pane" => "[-x width] [-y height] [-t target-pane]",
        "run-shell" => "[-b] shell-command",
        "save-buffer" => "path",
        "select-layout" => "[-t target-pane] [layout-name]",
        "select-pane" => "[-t target-pane]",
        "send-keys" => "[-l] [-t target-pane] [key ...]",
        "set-environment" => "[-t target-session] variable [value]",
        "set-hook" => "[-agpRuw] [-t target-session] hook-name [command]",
        "set-option" => "[-t target-pane] option [value]",
        "set-window-option" => "[-t target-window] option [value]",
        "show-environment" => "[-t target-session] [variable]",
        "show-options" => "[-t target-pane] [option]",
        "show-window-options" => "[-t target-window] [option]",
        "split-window" => "[-dfhvP] [-F format] [-c start-directory] [-t target] [shell-command]",
        "wait-for" => "[-S] channel",
        _ => "",
    }
}

fn current_command(command: &str) -> String {
    command
        .split_whitespace()
        .next()
        .map(|s| s.rsplit('/').next().unwrap_or(s).to_string())
        .unwrap_or_else(|| "sh".to_string())
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
            text => {
                if let Some(byte) = control_key_byte(text) {
                    out.push(byte);
                } else {
                    out.extend_from_slice(text.as_bytes());
                }
            }
        }
    }
    out
}

fn control_key_byte(key: &str) -> Option<u8> {
    let suffix = key.strip_prefix("C-")?;
    let mut chars = suffix.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    match ch {
        '@' | ' ' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        ch if ch.is_ascii_alphabetic() => Some((ch.to_ascii_uppercase() as u8) & 0x1f),
        _ => None,
    }
}

// ──────────────────────────────────────────────────────────────────────────
// cmux status pill sink (설계 §3.2/§4.4 — diff/apply, CLI 경로)
//
// understatus가 `--surface-format cmux-status`로 내는 pill JSON 1줄을 받아 직전
// 적용 상태와 diff한 뒤, 변경분만 `cmux set-status/clear-status`로
// 구동한다. 본 모듈은 sink 자체(파싱·diff·apply·Drop·서킷브레이커)만 담는다 —
// 소비점(client.rs) 배선은 후속 청크(C6)에서 한다.
//
// C6에서 소비점(client.rs)이 sink를 owned 로컬로 생성·보유하고 `apply`를 호출하므로
// 공개 API(`CmuxStatusSink::new`/`apply`/`reconcile_orphans`/`Drop`)와 그 의존 내부가
// 비-test 빌드에서 실제로 호출된다. 따라서 C5의 모듈 단위 `#![allow(dead_code)]`는 제거됐다.
//
// 이 re-export는 C6 소비점(client.rs)이 `crate::tmux_compat::CmuxStatusSink`로 참조하는 진입점이다.
pub(crate) use cmux_status::CmuxStatusSink;

mod cmux_status {
    use super::{
        CmuxSurfaceContext, Command, client, is_valid_cmux_ref_segment, run_cmux_command, sanitize,
    };
    use serde::Deserialize;
    use std::collections::BTreeMap;

    /// 연속 cmux 호출 실패가 이 횟수에 도달하면 sink를 비표시(blackout)로 전환한다(설계 §4.4).
    const CMUX_STATUS_FAILURE_LIMIT: u32 = 3;

    /// understatus가 내는 pill JSON의 최상위 스키마 식별자(설계 §3.3). 이 값이 아니면 no-op.
    const CMUX_STATUS_SCHEMA: &str = "cmux-status";

    /// pill 텍스트 필드(value/label/icon) 길이 상한(문자 수). cmux-bound 문자열을 제한해
    /// 적대적/버그성 초장문이 cmux argv·status 라인을 오염시키지 못하게 한다(Codex HIGH Issue 2+5).
    const CMUX_STATUS_FIELD_MAX_CHARS: usize = 256;

    /// pill key 세그먼트 길이 상한(문자 수). 키도 cmux argv·status 라인에 들어가므로 초장문을
    /// 차단한다(Codex HIGH Issue 2). 키 id는 짧은 식별자(model/ctx 등)라 64면 넉넉하다.
    const CMUX_STATUS_KEY_MAX_CHARS: usize = 64;

    /// understatus(`--surface-format cmux-status`)가 stdout 1줄로 내는 pill 페이로드.
    ///
    /// 미상 필드는 무시하고(`serde(default)`로 관대 처리), 역직렬화 실패는 상위 `apply`에서
    /// no-op으로 흡수한다(설계 §3.3 additive-optional 계약). `key`는 prefix 없는 세그먼트
    /// id이며, lterm이 `key_prefix`를 앞에 붙인다.
    #[derive(Debug, Clone, Default, Deserialize)]
    #[serde(default)]
    struct CmuxStatusInput {
        /// 스키마 식별자. `"cmux-status"`가 아니면 sink는 no-op 처리한다.
        schema: String,
        /// 페이로드 버전. 현재는 관대 처리(하드게이트 안 함).
        #[allow(dead_code)]
        version: u32,
        /// pill 목록(model/ctx/cpu/mem 등). 소스 없는 세그먼트는 애초에 빠져 온다.
        pills: Vec<PillIn>,
    }

    /// 단일 pill의 역직렬화 입력(설계 §3.3 stdout 계약).
    #[derive(Debug, Clone, Default, Deserialize)]
    #[serde(default)]
    struct PillIn {
        /// prefix 없는 세그먼트 id(`model`/`ctx`/`cpu`/`mem`).
        key: String,
        /// pill 라벨(선택). cmux pill 텍스트의 접두 라벨.
        label: Option<String>,
        /// pill 값 문자열(예 `"ctx 42%"`).
        value: String,
        /// pill 색(`#RRGGBB`). 없으면 cmux 기본색.
        color: Option<String>,
        /// pill 아이콘명(선택). 미지원명은 cmux가 무시.
        icon: Option<String>,
        /// pill 우선순위(높을수록 앞). cmux `--priority`.
        priority: u8,
    }

    /// 직전에 적용된 단일 pill 상태(diff 비교용). full key(prefix+id) → 이 값.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct PillState {
        /// pill 값 문자열. 문자열 그대로 비교한다.
        value: String,
        /// pill 색(`#RRGGBB`).
        color: Option<String>,
        /// pill 아이콘명.
        icon: Option<String>,
        /// pill 우선순위.
        priority: u8,
        /// pill 라벨.
        label: Option<String>,
    }

    /// diff가 산출하는 cmux 명령(순수 표현). 실제 실행은 [`CmuxCommandRunner`]가 맡는다.
    ///
    /// 명령 실행 경로를 enum으로 분리해 순수 `plan_commands`는 직접 단언하고, `apply`의 실행
    /// 경로는 테스트 collector mock으로 검증할 수 있게 한다(설계 §4.4 apply 레이어 분리).
    ///
    /// progress(`set-progress`/`clear-progress`)는 cmux에서 워크스페이스 전역 + list-progress
    /// 부재라 SIGKILL 시 누수·다중 pane 클로버 위험이 있어 전면 제거했다(Codex HIGH Issue 3).
    /// ctx는 pill(`set-status`)로만 표현한다.
    #[derive(Debug, Clone, PartialEq)]
    enum CmuxCommand {
        /// `cmux set-status <key> <value> [--icon][--color][--priority]`.
        SetStatus {
            key: String,
            value: String,
            color: Option<String>,
            icon: Option<String>,
            priority: u8,
            label: Option<String>,
        },
        /// `cmux clear-status <key>`.
        ClearStatus { key: String },
    }

    /// `applied`/`desired` pill 맵을 diff해 결정적 순서의 명령 리스트를 만든다(순수).
    ///
    /// 규칙(설계 §4.4):
    /// - desired에 있고 applied와 다르거나 없음 → `SetStatus`.
    /// - applied에 있고 desired에 없음 → `ClearStatus`.
    /// - 무변경 → 생략(유휴 스폰 0).
    ///
    /// `BTreeMap` 순회로 명령 순서가 결정적이다(SetStatus/ClearStatus 모두 키 사전순).
    fn plan_commands(
        applied: &BTreeMap<String, PillState>,
        desired: &BTreeMap<String, PillState>,
    ) -> Vec<CmuxCommand> {
        let mut commands = Vec::new();

        // desired 기준: 신규/변경분 → SetStatus.
        for (key, state) in desired {
            if applied.get(key) != Some(state) {
                commands.push(CmuxCommand::SetStatus {
                    key: key.clone(),
                    value: state.value.clone(),
                    color: state.color.clone(),
                    icon: state.icon.clone(),
                    priority: state.priority,
                    label: state.label.clone(),
                });
            }
        }

        // applied 기준: desired에서 사라진 키 → ClearStatus.
        for key in applied.keys() {
            if !desired.contains_key(key) {
                commands.push(CmuxCommand::ClearStatus { key: key.clone() });
            }
        }

        commands
    }

    /// cmux 명령 실행 추상화. 프로덕션은 CLI 서브프로세스로, 테스트는 collector mock으로 주입한다.
    ///
    /// `run`은 명령 실행 성공 여부(`true`=성공)만 돌려준다. sink는 이 결과로 서킷브레이커를
    /// 카운트하고, 성공한 명령만 `applied`에 반영한다(설계 §4.4, 실패분은 다음 tick 재시도).
    trait CmuxCommandRunner {
        /// 단일 cmux 명령을 실행한다. 워크스페이스 타깃 인자는 `surface`로 주어진다.
        /// 반환값: 명령이 성공적으로 적용됐으면 `true`.
        fn run(&mut self, command: &CmuxCommand, surface: &CmuxSurfaceContext) -> bool;
    }

    /// set-status류 cmux 명령에 워크스페이스 타깃만 부여하는 신규 인자 빌더(설계 §4.4).
    ///
    /// `add_cmux_surface_context_args`(`--surface`/`--window`도 방출)는 set-status가 거부할 수
    /// 있어 재사용하지 않는다. workspace_ref가 있을 때만 `--workspace <ref>`를 방출한다.
    fn add_cmux_status_target_args(command: &mut Command, surface: &CmuxSurfaceContext) {
        if let Some(workspace_ref) = surface.workspace_ref.as_deref() {
            command.arg("--workspace").arg(workspace_ref);
        }
    }

    /// 프로덕션 runner: 각 [`CmuxCommand`]를 실제 `cmux` 서브프로세스로 실행한다(`run_cmux_command` 경유).
    struct CliCmuxCommandRunner;

    impl CliCmuxCommandRunner {
        /// `CmuxCommand`를 `cmux` 인자 벡터로 빌드한다(워크스페이스 타깃 포함).
        fn build_command(command: &CmuxCommand, surface: &CmuxSurfaceContext) -> Command {
            let mut cmd = Command::new("cmux");
            match command {
                CmuxCommand::SetStatus {
                    key,
                    value,
                    color,
                    icon,
                    priority,
                    label,
                } => {
                    cmd.arg("set-status").arg(key).arg(value);
                    if let Some(label) = label {
                        cmd.arg("--label").arg(label);
                    }
                    if let Some(icon) = icon {
                        cmd.arg("--icon").arg(icon);
                    }
                    if let Some(color) = color {
                        cmd.arg("--color").arg(color);
                    }
                    cmd.arg("--priority").arg(priority.to_string());
                    add_cmux_status_target_args(&mut cmd, surface);
                }
                CmuxCommand::ClearStatus { key } => {
                    cmd.arg("clear-status").arg(key);
                    add_cmux_status_target_args(&mut cmd, surface);
                }
            }
            cmd
        }
    }

    impl CmuxCommandRunner for CliCmuxCommandRunner {
        fn run(&mut self, command: &CmuxCommand, surface: &CmuxSurfaceContext) -> bool {
            let mut cmd = Self::build_command(command, surface);
            match run_cmux_command(&mut cmd) {
                Ok(output) => output.status.success(),
                Err(_) => false,
            }
        }
    }

    /// cmux 네이티브 status pill sink(설계 §3.2). understatus pill JSON을 받아 diff 후 cmux를 구동한다.
    ///
    /// 직전 적용 상태(`applied`)를 보관해 변경분만 스폰한다(유휴 스폰 0).
    /// 연속 실패가 [`CMUX_STATUS_FAILURE_LIMIT`]에 도달하면 `healthy=false`로 비표시(blackout)
    /// 전환한다(설계 §4.4 서킷브레이커). `Drop` 시 자기 키를 전부 청소한다(누수 1차 방어, §4.3).
    pub(crate) struct CmuxStatusSink {
        /// attach 시점 1회 캡처한 워크스페이스 컨텍스트(`cmux_status_identity` 결과).
        workspace: CmuxSurfaceContext,
        /// pane 스코프 키 prefix(`lterm.<sanitized-pane>.`). 다중 세션 키 충돌 차단.
        key_prefix: String,
        /// 직전 적용된 pill 상태(full key → 상태). diff 기준.
        applied: BTreeMap<String, PillState>,
        /// 생성 시점 `cmux` 바이너리 존재 여부. false면 모든 명령을 skip한다.
        cmux_available: bool,
        /// 연속 실패 카운트. 성공 시 0으로 리셋.
        consecutive_failures: u32,
        /// 서킷브레이커 상태. false면 명령을 발행하지 않는다(blackout).
        healthy: bool,
    }

    impl CmuxStatusSink {
        /// sink를 생성한다(설계 §3.2 `new`).
        ///
        /// `key_prefix`는 `lterm.<sanitized-pane>.` 형태로, pane id를 [`is_valid_cmux_ref_segment`]
        /// 통과 문자만 남기게 정규화한다(설계 §6 키 sanitization). `cmux_available`은 생성 시점
        /// `command_exists("cmux")`로 정한다.
        ///
        /// 트레이드오프(Codex MEDIUM Issue 4 — 동시 attach): key_prefix는 pane id만으로 스코프되며
        /// instance id를 섞지 않는다. 이는 의도적이다 — SIGKILL/`panic=abort` 후 고아 재조정
        /// ([`reconcile_orphans`])이 동작하려면 prefix가 프로세스 인스턴스에 독립이어야(같은 pane이면
        /// 재기동 후에도 같은 prefix여야) 직전 잔재를 식별할 수 있기 때문이다. 같은 pane에 lterm
        /// attach가 동시에 2개 붙는 상황은 (a) 드물고 (b) 같은 세션이라 동일 pill을 쓰며 (c) 최악이
        /// 짧은 깜빡임(서로의 동일 set-status를 덮어쓰는 정도)으로, 교차 손상이 아니다.
        pub(crate) fn new(workspace: CmuxSurfaceContext, pane_id: &str) -> Self {
            let key_prefix = format!("lterm.{}.", sanitize_cmux_pane_segment(pane_id));
            Self {
                workspace,
                key_prefix,
                applied: BTreeMap::new(),
                cmux_available: client::command_exists("cmux"),
                consecutive_failures: 0,
                healthy: true,
            }
        }

        /// understatus가 낸 pill JSON 1줄을 적용한다(설계 §3.2/§4.4).
        ///
        /// 1. JSON 파싱 실패 또는 schema 불일치 → no-op(구버전 oneline 등 무해 흡수).
        /// 2. desired 맵 구성(full key=`key_prefix`+pill.key) → `plan_commands`로 diff.
        /// 3. `healthy && cmux_available`일 때만 각 명령 실행. 성공분만 `applied`에 반영하고
        ///    (실패분은 다음 tick 재시도), 연속 실패가 한계에 도달하면 blackout.
        ///
        /// understatus가 보내는 `progress` 필드는 더 이상 처리하지 않는다(미지 필드로 무시).
        /// progress는 워크스페이스 전역 누수 위험으로 제거됐다(Codex HIGH Issue 3, 설계 §4.3).
        pub(crate) fn apply(&mut self, stdout_line: &str) {
            let mut runner = CliCmuxCommandRunner;
            self.apply_with_runner(stdout_line, &mut runner);
        }

        /// [`CmuxStatusSink::apply`]의 runner 주입판(테스트용 collector mock 경로).
        fn apply_with_runner<R: CmuxCommandRunner>(&mut self, stdout_line: &str, runner: &mut R) {
            let Ok(input) = serde_json::from_str::<CmuxStatusInput>(stdout_line) else {
                // 구버전 oneline SGR 등 비-JSON 수신 → 무해 no-op(설계 ADR-4).
                return;
            };
            if input.schema != CMUX_STATUS_SCHEMA {
                return;
            }

            let desired = self.build_desired_pills(&input);
            let commands = plan_commands(&self.applied, &desired);

            if commands.is_empty() {
                return;
            }
            if !self.cmux_available || !self.healthy {
                return;
            }

            self.run_commands(&commands, &desired, runner);
        }

        /// pill 입력을 full-key(`key_prefix`+id) → [`PillState`] 맵으로 변환한다.
        ///
        /// 검증(Codex HIGH Issue 2+5): `pill.key`는 [`is_valid_pill_key`]
        /// (`[A-Za-z0-9_-]` AND `<= CMUX_STATUS_KEY_MAX_CHARS`)만 허용하고, 미통과 키는
        /// skip한다(`.`/공백/`=`/제어문자/빈/초장문 차단). 안전 key만 써야 `list-status` 파서
        /// 가정(키 토큰에 `=`·공백 없음)이 성립한다. value/label/icon은
        /// [`CMUX_STATUS_FIELD_MAX_CHARS`]로 cap하고, icon은 ref-segment 문법만, color는
        /// `#RRGGBB`만 허용한다(아니면 해당 필드를 떨어뜨린다).
        fn build_desired_pills(&self, input: &CmuxStatusInput) -> BTreeMap<String, PillState> {
            let mut desired = BTreeMap::new();
            for pill in &input.pills {
                if !is_valid_pill_key(&pill.key) {
                    // `.`/공백/`=`/제어문자/빈 문자열/초장문 등 미허용 key → skip.
                    if cmux_status_debug_enabled() {
                        eprintln!(
                            "warning: cmux status pill key {:?} rejected (not [A-Za-z0-9_-] or too long); skipping",
                            sanitize::terminal_text(&pill.key)
                        );
                    }
                    continue;
                }
                let full_key = format!("{}{}", self.key_prefix, pill.key);
                desired.insert(
                    full_key,
                    PillState {
                        value: cap_field(&pill.value),
                        color: sanitize_pill_color(pill.color.as_deref()),
                        icon: sanitize_pill_icon(pill.icon.as_deref()),
                        priority: pill.priority,
                        label: pill.label.as_deref().map(cap_field),
                    },
                );
            }
            desired
        }

        /// 계획된 명령을 실행하고, 성공분만 적용 상태에 반영하며 서킷브레이커를 갱신한다.
        fn run_commands<R: CmuxCommandRunner>(
            &mut self,
            commands: &[CmuxCommand],
            desired: &BTreeMap<String, PillState>,
            runner: &mut R,
        ) {
            for command in commands {
                let ok = runner.run(command, &self.workspace);
                if ok {
                    self.consecutive_failures = 0;
                    self.commit_applied(command, desired);
                } else {
                    self.record_failure();
                    if !self.healthy {
                        // blackout 전환 — 남은 명령은 발행하지 않는다.
                        return;
                    }
                }
            }
        }

        /// 성공한 단일 명령을 `applied`에 반영한다(실패분은 미반영=재시도).
        fn commit_applied(&mut self, command: &CmuxCommand, desired: &BTreeMap<String, PillState>) {
            match command {
                CmuxCommand::SetStatus { key, .. } => {
                    if let Some(state) = desired.get(key) {
                        self.applied.insert(key.clone(), state.clone());
                    }
                }
                CmuxCommand::ClearStatus { key } => {
                    self.applied.remove(key);
                }
            }
        }

        /// 연속 실패를 1 증가시키고, 한계 도달 시 blackout(`healthy=false`)으로 전환하며 1회 경고한다.
        fn record_failure(&mut self) {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            if self.consecutive_failures >= CMUX_STATUS_FAILURE_LIMIT && self.healthy {
                self.healthy = false;
                if cmux_status_debug_enabled() {
                    eprintln!(
                        "warning: cmux status pill sink disabled after {} consecutive failures; \
                     pills hidden (blackout) for {}",
                        CMUX_STATUS_FAILURE_LIMIT,
                        sanitize::terminal_text(&self.key_prefix)
                    );
                }
            }
        }
    }

    impl Drop for CmuxStatusSink {
        /// 누수 1차 방어(설계 §4.3): 적용된 전 키를 `clear-status`한다.
        /// best-effort(`ManagedAttachGuard::drop` 모델) — 실패는 무시한다.
        /// `cmux_available=false`면 청소할 게 없으므로 skip한다.
        fn drop(&mut self) {
            if !self.cmux_available {
                return;
            }
            let mut runner = CliCmuxCommandRunner;
            self.cleanup_with_runner(&mut runner);
        }
    }

    impl CmuxStatusSink {
        /// [`Drop`]의 runner 주입판(테스트용). 적용 키 전체 clear-status를 발행한다.
        fn cleanup_with_runner<R: CmuxCommandRunner>(&mut self, runner: &mut R) {
            let keys: Vec<String> = self.applied.keys().cloned().collect();
            for key in keys {
                let _ = runner.run(&CmuxCommand::ClearStatus { key }, &self.workspace);
            }
            self.applied.clear();
        }

        /// 고아 pill 재조정(누수 2차 방어, 설계 §4.3 — attach 시 필수).
        ///
        /// 이번 세션이 아직 쓰지 않은 직전 하드킬/abort 잔재(SIGKILL·`panic=abort` 복구의 유일
        /// 경로)를 청소한다. `cmux list-status`(워크스페이스 한정)로 현재 키 집합을 조회해 자기
        /// `key_prefix`로 시작하는 키를 전부 `clear-status`한다. `applied`가 비어 있는 sink 생성
        /// 직후 호출이 전제이므로 이번 세션 desired는 아직 없고, 매치되는 prefix 키는 모두 잔재다.
        ///
        /// best-effort: `list-status` 자체가 실패하면(stale ref "Tab not found" 등) 재조정을
        /// 생략한다(복구 경로 실패가 sink 생성/동작을 막지 않음, 설계 §4.3 Critic gap).
        /// `cmux_available=false`거나 `healthy=false`면 조회 없이 즉시 반환한다.
        ///
        /// 트레이드오프(Codex MEDIUM Issue 4): 이 재조정은 prefix가 pane-keyed(인스턴스 무관)라는
        /// 전제 위에 성립한다. 같은 pane에 동시 attach가 2개면 한쪽 재조정이 다른 쪽의 막 쓴 키를
        /// 잠깐 지울 수 있으나, 다음 tick에서 `apply`가 desired를 재발행하므로 최악이 짧은 깜빡임이다
        /// (교차 손상 아님). 자세한 근거는 [`CmuxStatusSink::new`] 주석 참조.
        pub(crate) fn reconcile_orphans(&mut self) {
            if !self.cmux_available || !self.healthy {
                return;
            }
            let mut runner = CliCmuxCommandRunner;
            self.reconcile_orphans_with_runner(&mut runner);
        }

        /// [`CmuxStatusSink::reconcile_orphans`]의 list-runner 주입판(테스트용 collector mock 경로).
        fn reconcile_orphans_with_runner<R: CmuxStatusListRunner>(&mut self, runner: &mut R) {
            // list-status 실패(stale ref 등)는 best-effort로 재조정 생략(설계 §4.3).
            let Some(keys) = runner.list_status(&self.workspace) else {
                return;
            };
            let orphans = orphan_keys_to_clear(&keys, &self.key_prefix);
            for key in orphans {
                let _ = runner.run(&CmuxCommand::ClearStatus { key }, &self.workspace);
            }
        }
    }

    /// `cmux list-status` 실행 + clear-status 발행을 추상화한다(테스트 mock 주입용).
    ///
    /// 재조정은 `list-status` 조회(키 집합 산출)와 `clear-status` 발행 두 동작이 필요하다.
    /// `list_status`는 조회 성공 시 `Some(키 목록)`, 실패(stale ref·비-cmux 등) 시 `None`을 돌려준다.
    trait CmuxStatusListRunner: CmuxCommandRunner {
        /// 워크스페이스 한정 `cmux list-status`를 실행해 현재 status 키 목록을 돌려준다.
        /// 조회가 실패하면 `None`(best-effort 재조정 생략 신호).
        fn list_status(&mut self, surface: &CmuxSurfaceContext) -> Option<Vec<String>>;
    }

    impl CmuxStatusListRunner for CliCmuxCommandRunner {
        fn list_status(&mut self, surface: &CmuxSurfaceContext) -> Option<Vec<String>> {
            let mut cmd = Command::new("cmux");
            cmd.arg("list-status");
            add_cmux_status_target_args(&mut cmd, surface);
            let output = run_cmux_command(&mut cmd).ok()?;
            if !output.status.success() {
                // "Tab not found"류 stale ref 실패 → best-effort 생략.
                return None;
            }
            let text = String::from_utf8_lossy(&output.stdout.bytes);
            Some(parse_cmux_list_status_keys(&text))
        }
    }

    /// `cmux list-status` 출력에서 status 키만 추출한다(순수, 설계 §4.3 / AC 자동 1).
    ///
    /// 출력 1줄 형식은 `key=value icon=<name> color=<#hex> priority=<n>`(설계 §0.1)이며,
    /// 각 줄의 첫 `key=` 토큰의 키 부분만 취한다. 빈 출력/"No status entries"/"Tab not found"류
    /// 비-키 줄(`=` 없음)은 건너뛰어 빈 vec를 낸다(에러 아님). 키가 빈 줄도 건너뛴다.
    ///
    /// 방어적으로 키 문법(dot-구분 세그먼트 전부 `[A-Za-z0-9_-]`)에 맞지 않는 줄도 건너뛴다
    /// (Codex HIGH Issue 5). writer가 안전 key만 쓰므로 정상 잔재는 모두 통과하고, 메시지/오염
    /// 줄만 추가로 걸러진다.
    fn parse_cmux_list_status_keys(output: &str) -> Vec<String> {
        let mut keys = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // 첫 공백 전까지가 `key=value` 토큰. `=`가 없으면 키 줄이 아니므로 건너뛴다
            // ("No status entries"/"Tab not found" 등 메시지 줄 흡수).
            let Some(first_token) = trimmed.split_whitespace().next() else {
                continue;
            };
            let Some((key, _value)) = first_token.split_once('=') else {
                continue;
            };
            if !is_valid_cmux_status_key(key) {
                continue;
            }
            keys.push(key.to_string());
        }
        keys
    }

    /// status 키가 dot-구분 세그먼트 전부 [`is_valid_cmux_ref_segment`]를 통과하는지 검사한다.
    ///
    /// writer가 쓰는 full key(`lterm.<pane>.<id>`)는 점 세그먼트가 전부 안전 문자이므로 통과하고,
    /// 빈 키·점만 있는 키·비-안전 문자 키는 거른다(방어적 파싱, Codex HIGH Issue 5).
    fn is_valid_cmux_status_key(key: &str) -> bool {
        !key.is_empty() && key.split('.').all(is_valid_cmux_ref_segment)
    }

    /// `list-status` 키 집합 중 `key_prefix`로 시작하는 고아 키를 산출한다(순수, 설계 §4.3 / AC 자동 2).
    ///
    /// sink 생성 직후(이번 세션 desired 없음) 호출이므로, prefix 매치 키는 전부 직전 세션 잔재다.
    /// 입력 순서를 보존하고 중복은 그대로 둔다(clear-status는 멱등이라 무해).
    fn orphan_keys_to_clear(list_status_keys: &[String], key_prefix: &str) -> Vec<String> {
        list_status_keys
            .iter()
            .filter(|key| key.starts_with(key_prefix))
            .cloned()
            .collect()
    }

    /// pill key가 ref-segment 문법(`[A-Za-z0-9_-]`, 비어 있지 않음) AND 길이 상한을 만족하는지 검사한다.
    ///
    /// [`is_valid_cmux_ref_segment`]만으로는 길이를 막지 못하므로(초장문 all-`x` 키 통과)
    /// [`CMUX_STATUS_KEY_MAX_CHARS`] cap을 더한다(Codex HIGH Issue 2).
    fn is_valid_pill_key(key: &str) -> bool {
        key.chars().count() <= CMUX_STATUS_KEY_MAX_CHARS && is_valid_cmux_ref_segment(key)
    }

    /// pill 텍스트 필드(value/label)를 [`CMUX_STATUS_FIELD_MAX_CHARS`] 문자로 cap한다.
    ///
    /// cmux로 넘기는 문자열 길이를 제한해 적대적/버그성 초장문이 argv·status 라인을 오염시키지
    /// 못하게 한다(Codex HIGH Issue 2+5). 바이트가 아닌 문자(char) 경계로 잘라 UTF-8을 유지한다.
    fn cap_field(value: &str) -> String {
        value.chars().take(CMUX_STATUS_FIELD_MAX_CHARS).collect()
    }

    /// pill color 입력을 `#RRGGBB`(6 hex)만 통과시킨다(아니면 `None`).
    ///
    /// 색 인자를 엄격히 검증해 임의 문자열이 cmux `--color`로 흘러드는 것을 막는다.
    fn sanitize_pill_color(color: Option<&str>) -> Option<String> {
        let color = color?;
        let valid = color.len() == 7
            && color.starts_with('#')
            && color[1..].chars().all(|ch| ch.is_ascii_hexdigit());
        valid.then(|| color.to_string())
    }

    /// pill icon 입력을 ref-segment 문법(`[A-Za-z0-9_-]`, 비어 있지 않음)만 통과시킨다(아니면 `None`).
    ///
    /// icon명을 안전 문자로 제한해 제어문자·공백·초장문이 cmux `--icon`으로 흘러드는 것을 막는다.
    /// 추가로 [`CMUX_STATUS_FIELD_MAX_CHARS`]로 cap한다(ref-segment 검증과 중복 안전망).
    fn sanitize_pill_icon(icon: Option<&str>) -> Option<String> {
        let icon = icon?;
        let capped = cap_field(icon);
        is_valid_cmux_ref_segment(&capped).then_some(capped)
    }

    /// pane id를 cmux 키 세그먼트 안전 문자만 남기게 정규화한다(설계 §6).
    ///
    /// [`is_valid_cmux_ref_segment`] 허용 집합(`[A-Za-z0-9_-]`) 외 문자는 `_`로 치환한다.
    /// 예: pane `"%3"` → `"_3"`. 빈 결과는 `"_"`로 폴백해 키 prefix가 비지 않게 한다.
    fn sanitize_cmux_pane_segment(pane_id: &str) -> String {
        let sanitized: String = pane_id
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        if sanitized.is_empty() {
            "_".to_string()
        } else {
            sanitized
        }
    }

    /// 서킷브레이커 경고를 stderr로 낼지 여부(`LTERM_STATUS_DEBUG` 가드). 기본 false(조용히 blackout).
    fn cmux_status_debug_enabled() -> bool {
        std::env::var_os("LTERM_STATUS_DEBUG").is_some_and(|value| {
            let value = value.to_string_lossy();
            let trimmed = value.trim();
            !(trimmed.is_empty()
                || trimmed.eq_ignore_ascii_case("0")
                || trimmed.eq_ignore_ascii_case("false")
                || trimmed.eq_ignore_ascii_case("no")
                || trimmed.eq_ignore_ascii_case("off"))
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        // 키 정규화 단언은 부모 모듈의 cmux ref 검증기를 그대로 재사용한다.
        use crate::tmux_compat::is_valid_cmux_ref_segment;

        /// 테스트용 명령 collector mock. 실행된 명령을 순서대로 기록하고, 성공/실패를
        /// 구성 가능하게 한다(서킷브레이커 검증용).
        struct CollectingRunner {
            commands: Vec<CmuxCommand>,
            /// false면 모든 `run`이 실패(서킷브레이커 유발)로 간주된다.
            succeed: bool,
        }

        impl CollectingRunner {
            fn new() -> Self {
                Self {
                    commands: Vec::new(),
                    succeed: true,
                }
            }

            fn always_failing() -> Self {
                Self {
                    commands: Vec::new(),
                    succeed: false,
                }
            }
        }

        impl CmuxCommandRunner for CollectingRunner {
            fn run(&mut self, command: &CmuxCommand, _surface: &CmuxSurfaceContext) -> bool {
                self.commands.push(command.clone());
                self.succeed
            }
        }

        /// 재조정 테스트용 mock. `list_status`가 돌려줄 키 목록(또는 실패 시 `None`)을 미리
        /// 구성하고, 실행된 `clear-status` 명령을 순서대로 기록한다.
        struct ListingRunner {
            /// `list_status`가 돌려줄 값. `None`이면 list-status 실패(stale ref)를 모사.
            list_result: Option<Vec<String>>,
            /// 재조정 중 발행된 명령 기록(전부 clear-status여야 한다).
            commands: Vec<CmuxCommand>,
        }

        impl ListingRunner {
            fn with_keys(keys: &[&str]) -> Self {
                Self {
                    list_result: Some(keys.iter().map(|key| key.to_string()).collect()),
                    commands: Vec::new(),
                }
            }

            fn failing_list() -> Self {
                Self {
                    list_result: None,
                    commands: Vec::new(),
                }
            }
        }

        impl CmuxCommandRunner for ListingRunner {
            fn run(&mut self, command: &CmuxCommand, _surface: &CmuxSurfaceContext) -> bool {
                self.commands.push(command.clone());
                true
            }
        }

        impl CmuxStatusListRunner for ListingRunner {
            fn list_status(&mut self, _surface: &CmuxSurfaceContext) -> Option<Vec<String>> {
                self.list_result.clone()
            }
        }

        fn test_surface() -> CmuxSurfaceContext {
            CmuxSurfaceContext {
                surface_ref: "surface:uuid-test".to_string(),
                workspace_ref: Some("workspace:uuid-test".to_string()),
                window_ref: Some("window:uuid-test".to_string()),
            }
        }

        fn pill_state(value: &str) -> PillState {
            PillState {
                value: value.to_string(),
                color: None,
                icon: None,
                priority: 50,
                label: None,
            }
        }

        /// AC1: 무변경 → 빈 vec.
        #[test]
        fn plan_commands_no_change_is_empty() {
            let mut map = BTreeMap::new();
            map.insert("lterm._3.model".to_string(), pill_state("gpt-5.5"));
            let commands = plan_commands(&map, &map.clone());
            assert!(commands.is_empty());
        }

        /// AC1: 값 변경 → SetStatus.
        #[test]
        fn plan_commands_value_change_emits_set_status() {
            let mut applied = BTreeMap::new();
            applied.insert("lterm._3.model".to_string(), pill_state("gpt-5.5"));
            let mut desired = BTreeMap::new();
            desired.insert("lterm._3.model".to_string(), pill_state("gpt-6"));
            let commands = plan_commands(&applied, &desired);
            assert_eq!(
                commands,
                vec![CmuxCommand::SetStatus {
                    key: "lterm._3.model".to_string(),
                    value: "gpt-6".to_string(),
                    color: None,
                    icon: None,
                    priority: 50,
                    label: None,
                }]
            );
        }

        /// AC1: 제거 → ClearStatus.
        #[test]
        fn plan_commands_removal_emits_clear_status() {
            let mut applied = BTreeMap::new();
            applied.insert("lterm._3.ctx".to_string(), pill_state("ctx 42%"));
            let desired = BTreeMap::new();
            let commands = plan_commands(&applied, &desired);
            assert_eq!(
                commands,
                vec![CmuxCommand::ClearStatus {
                    key: "lterm._3.ctx".to_string()
                }]
            );
        }

        /// AC1: 추가 → SetStatus.
        #[test]
        fn plan_commands_addition_emits_set_status() {
            let applied = BTreeMap::new();
            let mut desired = BTreeMap::new();
            desired.insert("lterm._3.cpu".to_string(), pill_state("cpu 31%"));
            let commands = plan_commands(&applied, &desired);
            assert_eq!(commands.len(), 1);
            assert!(matches!(
                commands[0],
                CmuxCommand::SetStatus { ref key, .. } if key == "lterm._3.cpu"
            ));
        }

        /// AC1: 명령 순서 결정적(SetStatus는 키 사전순, ClearStatus 뒤).
        #[test]
        fn plan_commands_order_is_deterministic() {
            let mut applied = BTreeMap::new();
            applied.insert("lterm._3.zeta".to_string(), pill_state("old"));
            let mut desired = BTreeMap::new();
            desired.insert("lterm._3.beta".to_string(), pill_state("b"));
            desired.insert("lterm._3.alpha".to_string(), pill_state("a"));
            let commands = plan_commands(&applied, &desired);
            // alpha, beta(사전순 SetStatus) → zeta(ClearStatus).
            assert_eq!(commands.len(), 3);
            assert!(matches!(
                &commands[0],
                CmuxCommand::SetStatus { key, .. } if key == "lterm._3.alpha"
            ));
            assert!(matches!(
                &commands[1],
                CmuxCommand::SetStatus { key, .. } if key == "lterm._3.beta"
            ));
            assert!(matches!(
                &commands[2],
                CmuxCommand::ClearStatus { key } if key == "lterm._3.zeta"
            ));
        }

        /// AC3: 신규 빌더 — set-status argv에 `--workspace` 有 AND `--surface`/`--window` 無.
        #[test]
        fn set_status_command_emits_workspace_only_target() {
            let surface = test_surface();
            let command = CmuxCommand::SetStatus {
                key: "lterm._3.model".to_string(),
                value: "gpt-5.5".to_string(),
                color: Some("#7AA2F7".to_string()),
                icon: Some("sparkles".to_string()),
                priority: 60,
                label: None,
            };
            let cmd = CliCmuxCommandRunner::build_command(&command, &surface);
            let argv: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert!(argv.iter().any(|a| a == "--workspace"));
            assert!(argv.iter().any(|a| a == "workspace:uuid-test"));
            assert!(!argv.iter().any(|a| a == "--surface"));
            assert!(!argv.iter().any(|a| a == "--window"));
            // set-status 본 인자도 확인.
            assert_eq!(argv[0], "set-status");
            assert_eq!(argv[1], "lterm._3.model");
            assert_eq!(argv[2], "gpt-5.5");
        }

        /// AC3: workspace_ref 없으면 `--workspace`도 방출 안 함.
        #[test]
        fn target_args_omit_workspace_when_absent() {
            let surface = CmuxSurfaceContext {
                surface_ref: "surface:x".to_string(),
                workspace_ref: None,
                window_ref: None,
            };
            let mut cmd = Command::new("cmux");
            add_cmux_status_target_args(&mut cmd, &surface);
            assert_eq!(cmd.get_args().count(), 0);
        }

        /// AC4: apply non-JSON → no-op(applied 불변, 패닉 없음).
        #[test]
        fn apply_non_json_is_no_op() {
            let mut sink = CmuxStatusSink::new(test_surface(), "%3");
            sink.cmux_available = true;
            let mut runner = CollectingRunner::new();
            sink.apply_with_runner("\x1b[31msome oneline sgr\x1b[0m", &mut runner);
            assert!(runner.commands.is_empty());
            assert!(sink.applied.is_empty());
        }

        /// AC4: schema 불일치 → no-op.
        #[test]
        fn apply_wrong_schema_is_no_op() {
            let mut sink = CmuxStatusSink::new(test_surface(), "%3");
            sink.cmux_available = true;
            let mut runner = CollectingRunner::new();
            sink.apply_with_runner(
            r#"{"schema":"other","version":1,"pills":[{"key":"model","value":"x","priority":1}]}"#,
            &mut runner,
        );
            assert!(runner.commands.is_empty());
            assert!(sink.applied.is_empty());
        }

        /// AC5: 키 정규화 — pane "%3" → key_prefix가 `is_valid_cmux_ref_segment` 통과.
        #[test]
        fn key_prefix_segments_are_valid_cmux_refs() {
            let sink = CmuxStatusSink::new(test_surface(), "%3");
            // prefix = "lterm._3." → 각 점-구분 세그먼트가 유효해야 한다.
            assert_eq!(sink.key_prefix, "lterm._3.");
            for segment in sink.key_prefix.trim_end_matches('.').split('.') {
                assert!(
                    is_valid_cmux_ref_segment(segment),
                    "segment {segment:?} must be a valid cmux ref"
                );
            }
            // full key의 세그먼트도 유효(pill id append 후).
            let full = format!("{}model", sink.key_prefix);
            for segment in full.split('.') {
                assert!(is_valid_cmux_ref_segment(segment));
            }
        }

        /// AC5: sanitize는 비허용 문자를 `_`로 치환, 빈 결과는 `_` 폴백.
        #[test]
        fn sanitize_cmux_pane_segment_replaces_disallowed() {
            assert_eq!(sanitize_cmux_pane_segment("%3"), "_3");
            assert_eq!(sanitize_cmux_pane_segment("pane.1/x"), "pane_1_x");
            assert_eq!(sanitize_cmux_pane_segment("ok-id_9"), "ok-id_9");
            assert_eq!(sanitize_cmux_pane_segment(""), "_");
            assert_eq!(sanitize_cmux_pane_segment("%%"), "__");
        }

        /// pill JSON 1개를 만드는 헬퍼(검증 테스트용). key/value/color/icon을 지정한다.
        fn pill_payload(key: &str, value: &str, color: &str, icon: &str) -> String {
            format!(
                r#"{{"schema":"cmux-status","version":1,"pills":[
                {{"key":{key},"value":{value},"color":{color},"icon":{icon},"priority":50}}]}}"#,
                key = serde_json::to_string(key).unwrap(),
                value = serde_json::to_string(value).unwrap(),
                color = serde_json::to_string(color).unwrap(),
                icon = serde_json::to_string(icon).unwrap(),
            )
        }

        /// Codex HIGH Issue 2: 잘못된 pill key("a.b"/"a b"/"a=c"/제어문자/빈/초장문) → skip,
        /// 안전 key만 set-status 발행.
        #[test]
        fn invalid_pill_keys_are_skipped() {
            for bad_key in ["a.b", "a b", "a=c", "a\u{1}b", "", &"x".repeat(300)] {
                let mut sink = CmuxStatusSink::new(test_surface(), "%3");
                sink.cmux_available = true;
                let payload = pill_payload(bad_key, "v", "#7AA2F7", "spark");
                let mut runner = CollectingRunner::new();
                sink.apply_with_runner(&payload, &mut runner);
                assert!(
                    runner.commands.is_empty(),
                    "key {bad_key:?} should be skipped"
                );
                assert!(sink.applied.is_empty());
            }
            // 안전 key는 통과.
            let mut sink = CmuxStatusSink::new(test_surface(), "%3");
            sink.cmux_available = true;
            let payload = pill_payload("model", "gpt-5.5", "#7AA2F7", "spark");
            let mut runner = CollectingRunner::new();
            sink.apply_with_runner(&payload, &mut runner);
            assert_eq!(runner.commands.len(), 1);
            assert!(sink.applied.contains_key("lterm._3.model"));
        }

        /// Codex HIGH Issue 2: value/label은 256자로 cap.
        #[test]
        fn cap_field_caps_text_to_max_chars() {
            assert_eq!(cap_field("short").len(), 5);
            assert_eq!(cap_field(&"x".repeat(300)).chars().count(), 256);
            // 멀티바이트 문자도 char 경계로 안전하게 잘린다.
            let capped = cap_field(&"가".repeat(300));
            assert_eq!(capped.chars().count(), 256);
        }

        /// Codex HIGH Issue 2: color는 `#RRGGBB`만 통과, 그 외 None.
        #[test]
        fn sanitize_pill_color_accepts_only_hex6() {
            assert_eq!(
                sanitize_pill_color(Some("#7AA2F7")),
                Some("#7AA2F7".to_string())
            );
            assert_eq!(sanitize_pill_color(Some("#abc")), None);
            assert_eq!(sanitize_pill_color(Some("7AA2F7")), None);
            assert_eq!(sanitize_pill_color(Some("#7AA2F7; rm -rf")), None);
            assert_eq!(sanitize_pill_color(Some("#GGGGGG")), None);
            assert_eq!(sanitize_pill_color(None), None);
        }

        /// Codex HIGH Issue 2: icon은 ref-segment 문법만 통과(공백/제어문자/빈 → None).
        #[test]
        fn sanitize_pill_icon_accepts_only_ref_segment() {
            assert_eq!(
                sanitize_pill_icon(Some("sparkles")),
                Some("sparkles".to_string())
            );
            assert_eq!(sanitize_pill_icon(Some("spark les")), None);
            assert_eq!(sanitize_pill_icon(Some("spark\u{1}")), None);
            assert_eq!(sanitize_pill_icon(Some("")), None);
            assert_eq!(sanitize_pill_icon(None), None);
        }

        /// Codex HIGH Issue 2: 잘못된 color/icon은 떨어지고 value는 cap된 채 set-status 발행.
        #[test]
        fn build_desired_pills_drops_bad_color_icon_and_caps_value() {
            let sink = CmuxStatusSink::new(test_surface(), "%3");
            let payload = pill_payload("model", &"v".repeat(300), "not-a-color", "bad icon");
            let input: CmuxStatusInput = serde_json::from_str(&payload).unwrap();
            let desired = sink.build_desired_pills(&input);
            let state = desired.get("lterm._3.model").expect("valid key kept");
            assert_eq!(state.value.chars().count(), 256);
            assert!(state.color.is_none(), "invalid color dropped");
            assert!(state.icon.is_none(), "invalid icon dropped");
        }

        /// Codex HIGH Issue 5: 파서 방어 — 키 문법(dot-세그먼트 [A-Za-z0-9_-]) 외 줄은 skip.
        #[test]
        fn parse_list_status_skips_malformed_key_lines() {
            // 공백 포함 키·`.`만·제어문자 키 줄은 건너뛴다.
            let output = "lterm._3.model=opus priority=60\n\
                          bad key=value\n\
                          ..=value\n\
                          lterm._3.cpu=cpu 31% priority=100";
            let keys = parse_cmux_list_status_keys(output);
            assert_eq!(
                keys,
                vec!["lterm._3.model".to_string(), "lterm._3.cpu".to_string()]
            );
        }

        /// AC6: Drop → 모든 applied 키 clear-status 발행(collector mock).
        #[test]
        fn cleanup_emits_clear_for_all_applied() {
            let mut sink = CmuxStatusSink::new(test_surface(), "%3");
            sink.cmux_available = true;
            sink.applied
                .insert("lterm._3.model".to_string(), pill_state("gpt-5.5"));
            sink.applied
                .insert("lterm._3.cpu".to_string(), pill_state("cpu 31%"));
            let mut runner = CollectingRunner::new();
            sink.cleanup_with_runner(&mut runner);
            // 두 clear-status(사전순).
            assert_eq!(
                runner.commands,
                vec![
                    CmuxCommand::ClearStatus {
                        key: "lterm._3.cpu".to_string()
                    },
                    CmuxCommand::ClearStatus {
                        key: "lterm._3.model".to_string()
                    },
                ]
            );
            assert!(sink.applied.is_empty());
        }

        /// AC7: 서킷 — 3연속 실패 주입 → healthy=false, 이후 apply 명령 미발행.
        #[test]
        fn circuit_breaker_blackouts_after_three_failures() {
            let mut sink = CmuxStatusSink::new(test_surface(), "%3");
            sink.cmux_available = true;
            // 3개 pill을 desired로 — 전부 실패하면 3연속 실패로 blackout.
            let payload = r#"{"schema":"cmux-status","version":1,"pills":[
            {"key":"model","value":"gpt-5.5","priority":60},
            {"key":"cpu","value":"cpu 31%","priority":100},
            {"key":"mem","value":"mem 48%","priority":90}
        ]}"#;
            let mut runner = CollectingRunner::always_failing();
            sink.apply_with_runner(payload, &mut runner);
            // 3번째 실패에서 blackout → 남은 명령 없음(정확히 3개 시도, 전부 실패).
            assert_eq!(runner.commands.len(), 3);
            assert!(!sink.healthy);
            assert!(sink.applied.is_empty());

            // 이후 apply는 healthy=false라 명령 미발행.
            let mut runner2 = CollectingRunner::new();
            sink.apply_with_runner(payload, &mut runner2);
            assert!(runner2.commands.is_empty());
        }

        /// AC7 보강: 성공 시 연속 실패 카운트 리셋, healthy 유지, 성공분만 applied 반영.
        #[test]
        fn successful_apply_commits_state_and_keeps_healthy() {
            let mut sink = CmuxStatusSink::new(test_surface(), "%3");
            sink.cmux_available = true;
            // progress 필드는 더 이상 처리되지 않으나(미지 필드 무시) 역호환을 위해 포함해 둔다.
            let payload = r##"{"schema":"cmux-status","version":1,
            "pills":[{"key":"model","value":"gpt-5.5","color":"#7AA2F7","icon":"sparkles","priority":60}],
            "progress":{"value":0.42,"label":"ctx 42%"}}"##;
            let mut runner = CollectingRunner::new();
            sink.apply_with_runner(payload, &mut runner);
            // set-status(model)만 발행(progress 제거).
            assert_eq!(runner.commands.len(), 1);
            assert!(sink.healthy);
            assert_eq!(sink.consecutive_failures, 0);
            assert!(sink.applied.contains_key("lterm._3.model"));

            // 재적용(동일 payload) → 무변경 → 명령 0(유휴 스폰 0).
            let mut runner2 = CollectingRunner::new();
            sink.apply_with_runner(payload, &mut runner2);
            assert!(runner2.commands.is_empty());
        }

        /// 비-cmux 환경(cmux_available=false) → apply 명령 미발행(blackout, 설계 §6).
        #[test]
        fn apply_skips_when_cmux_unavailable() {
            let mut sink = CmuxStatusSink::new(test_surface(), "%3");
            sink.cmux_available = false;
            let payload = r#"{"schema":"cmux-status","version":1,"pills":[{"key":"model","value":"x","priority":1}]}"#;
            let mut runner = CollectingRunner::new();
            sink.apply_with_runner(payload, &mut runner);
            assert!(runner.commands.is_empty());
            assert!(sink.applied.is_empty());
        }

        // ── C6: list-status 파서 + 고아 재조정 ──

        /// AC 자동 1: list-status 파서 — 다중행 `key=value icon=.. color=.. priority=..` → 키 추출.
        #[test]
        fn parse_list_status_extracts_keys_from_multiline() {
            let output = "lterm._3.model=opus icon=sparkles color=#7AA2F7 priority=60\n\
                          lterm._3.cpu=cpu 31% color=#9ECE6A priority=100\n\
                          other.tool.build=ok priority=10";
            let keys = parse_cmux_list_status_keys(output);
            assert_eq!(
                keys,
                vec![
                    "lterm._3.model".to_string(),
                    "lterm._3.cpu".to_string(),
                    "other.tool.build".to_string(),
                ]
            );
        }

        /// AC 자동 1: 빈 출력 / "No status entries" / "Tab not found" → 빈 vec(에러 아님).
        #[test]
        fn parse_list_status_non_key_lines_yield_empty() {
            assert!(parse_cmux_list_status_keys("").is_empty());
            assert!(parse_cmux_list_status_keys("\n   \n").is_empty());
            assert!(parse_cmux_list_status_keys("No status entries").is_empty());
            assert!(parse_cmux_list_status_keys("Tab not found").is_empty());
            // `=`로 시작해 키가 빈 줄도 건너뛴다.
            assert!(parse_cmux_list_status_keys("=value priority=1").is_empty());
        }

        /// AC 자동 2: 고아 산출 — prefix 매치 키만 clear 대상, 비매치는 제외.
        #[test]
        fn orphan_keys_filters_by_prefix() {
            let keys = vec![
                "lterm._3.model".to_string(),
                "lterm._3.cpu".to_string(),
                "lterm._9.model".to_string(), // 다른 pane prefix → 제외
                "other.tool.build".to_string(), // 비-lterm → 제외
            ];
            let orphans = orphan_keys_to_clear(&keys, "lterm._3.");
            assert_eq!(
                orphans,
                vec!["lterm._3.model".to_string(), "lterm._3.cpu".to_string()]
            );
        }

        /// AC 자동 2: prefix 매치 없으면 빈 vec.
        #[test]
        fn orphan_keys_empty_when_no_prefix_match() {
            let keys = vec!["other.tool.build".to_string()];
            assert!(orphan_keys_to_clear(&keys, "lterm._3.").is_empty());
        }

        /// 재조정: 자기 prefix 잔재만 clear-status 발행(다른 prefix·다른 도구는 보존).
        #[test]
        fn reconcile_clears_only_own_prefix_orphans() {
            let mut sink = CmuxStatusSink::new(test_surface(), "%3");
            sink.cmux_available = true;
            let mut runner = ListingRunner::with_keys(&[
                "lterm._3.model",
                "lterm._3.cpu",
                "lterm._9.model",
                "claude_code",
            ]);
            sink.reconcile_orphans_with_runner(&mut runner);
            assert_eq!(
                runner.commands,
                vec![
                    CmuxCommand::ClearStatus {
                        key: "lterm._3.model".to_string()
                    },
                    CmuxCommand::ClearStatus {
                        key: "lterm._3.cpu".to_string()
                    },
                ]
            );
        }

        /// 재조정: list-status 실패(stale ref) → best-effort 생략(clear 미발행, 패닉 없음).
        #[test]
        fn reconcile_skips_when_list_status_fails() {
            let mut sink = CmuxStatusSink::new(test_surface(), "%3");
            sink.cmux_available = true;
            let mut runner = ListingRunner::failing_list();
            sink.reconcile_orphans_with_runner(&mut runner);
            assert!(runner.commands.is_empty());
        }

        /// 재조정: cmux 부재 → list-status 조회 없이 즉시 반환.
        #[test]
        fn reconcile_noop_when_cmux_unavailable() {
            let mut sink = CmuxStatusSink::new(test_surface(), "%3");
            sink.cmux_available = false;
            // reconcile_orphans(공개 진입)는 cmux_available=false면 list-runner를 만들지 않는다.
            sink.reconcile_orphans();
            // applied/healthy 불변 확인(부작용 없음).
            assert!(sink.applied.is_empty());
            assert!(sink.healthy);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn capture(names: &[&'static str]) -> Self {
            let saved = names
                .iter()
                .map(|name| (*name, std::env::var(name).ok()))
                .collect();
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: env-touching tests hold crate::TEST_ENV_LOCK and restore on drop.
            unsafe {
                for (name, value) in &self.saved {
                    match value {
                        Some(v) => std::env::set_var(name, v),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

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

    /// Codex HIGH Issue 1: 멈춘 명령(`sleep 30`)을 `wait_with_timeout`이 짧은 시한 내에
    /// `TimedOut`으로 끊는다. 호출자가 kill 책임이므로 여기서 kill+wait해 좀비를 회수한다.
    #[test]
    fn wait_with_timeout_kills_stuck_command() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let started = Instant::now();
        let result = wait_with_timeout(&mut child, Duration::from_millis(200));
        let elapsed = started.elapsed();
        // 데드라인 직후 타임아웃 에러를 돌려준다(넉넉히 2초 상한으로 폴링 지연 흡수).
        let err = result.expect_err("stuck command must time out");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout should fire promptly, took {elapsed:?}"
        );
        // 호출자 책임: 실제 kill로 좀비 회수.
        let _ = child.kill();
        let _ = child.wait();
    }

    /// 빠르게 끝나는 명령(`true`)은 타임아웃 전에 종료 코드를 돌려준다.
    #[test]
    fn wait_with_timeout_returns_status_for_fast_command() {
        let mut child = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn true");
        let status = wait_with_timeout(&mut child, Duration::from_secs(3)).expect("fast command");
        assert!(status.success());
    }

    #[test]
    fn maps_generic_control_keys() {
        assert_eq!(
            keys_to_bytes(
                &[
                    "C-a".into(),
                    "C-L".into(),
                    "C-]".into(),
                    "C-_".into(),
                    "C-?".into()
                ],
                false
            ),
            [0x01, 0x0c, 0x1d, 0x1f, 0x7f]
        );
        assert_eq!(keys_to_bytes(&["C-xy".into()], false), b"C-xy");
    }

    #[test]
    fn repeat_empty_send_payload_returns_without_spinning() {
        let repeated = repeated_send_payload(&[], usize::MAX).expect("empty repeated payload");
        assert!(repeated.is_empty());
    }

    #[test]
    fn repeat_send_payload_rejects_oversized_result() {
        let payload = vec![b'x'; MAX_SEND_DATA_BYTES / 2 + 1];
        let err = repeated_send_payload(&payload, 2).expect_err("oversized payload");
        assert!(err.to_string().contains("send data exceeds"));
    }

    #[test]
    fn target_matching_covers_tmux_forms_without_error_string_coupling() {
        let info = SessionInfo {
            id: "session-uuid".to_string(),
            name: "main".to_string(),
            pane_id: "%7".to_string(),
            parent_pane_id: None,
            parent_session_id: None,
            command: "sh".to_string(),
            cwd: "/tmp".to_string(),
            agent_name: None,
            created_unix_ms: 0,
            alive: true,
            exit_code: None,
            rows: 24,
            cols: 80,
            attached_clients: 0,
            process_id: None,
            process_group_id: None,
            status_theme: None,
        };

        assert!(target_matches_info("main", &info));
        assert!(target_matches_info("%7", &info));
        assert!(target_matches_info("7", &info));
        assert!(target_matches_info("session-uuid", &info));
        assert!(!target_matches_info("missing", &info));
    }

    #[test]
    fn show_option_accepts_clustered_value_flags() {
        assert!(show_option_prints_value(&args(["-gv", "status"])));
        assert!(show_option_prints_value(&args(["-qg", "status"])));
        assert!(!show_option_prints_value(&args(["status"])));
    }

    #[test]
    fn show_option_reports_focus_events_on_for_agent_tuis() {
        assert_eq!(
            show_option_name(&args(["-gqv", "focus-events"])).as_deref(),
            Some("focus-events")
        );
        assert_eq!(tmux_option_value(Some("focus-events")), "on");
        assert_eq!(tmux_option_value(Some("status")), "off");
        assert!(show_option_value_only(&args(["-gqv", "focus-events"])));
    }

    #[test]
    fn set_hook_accepts_omx_client_resized_hook_forms() {
        assert_eq!(
            set_hook(&args([
                "-t",
                "#{session_id}",
                "client-resized[867272301]",
                "run-shell",
                "-b",
                "tmux resize-pane -t %1 -y 2",
            ]))
            .expect("set client-resized hook"),
            0
        );
        assert_eq!(
            set_hook(&args([
                "-u",
                "-t",
                "#{session_id}",
                "client-resized[867272301]",
            ]))
            .expect("unset client-resized hook"),
            0
        );
        assert_eq!(
            set_hook(&args(["-ut#{session_id}", "client-resized[867272301]",]))
                .expect("clustered unset client-resized hook"),
            0
        );
        assert!(
            set_hook(&args(["-t"])).is_err(),
            "missing -t value should not silently consume the hook name"
        );
    }

    #[test]
    fn show_option_skips_target_values_before_option_name() {
        assert_eq!(
            show_option_name(&args(["-g", "-t", "%1", "focus-events"])).as_deref(),
            Some("focus-events")
        );
        assert_eq!(
            show_option_name(&args(["-gqt%1", "focus-events"])).as_deref(),
            Some("focus-events")
        );
        assert_eq!(
            show_option_name(&args(["-g", "--", "focus-events"])).as_deref(),
            Some("focus-events")
        );
        assert!(!show_option_value_only(&args(["-g", "focus-events"])));
    }

    #[test]
    fn display_popup_accepts_command_without_e_flag() {
        let temp = tempfile::tempdir().expect("tempdir");
        let output = temp.path().join("popup.out");
        let output_path = output.display().to_string();
        let quoted = shlex::try_quote(&output_path).expect("quote output");
        let status = display_popup(&[
            "sh".to_string(),
            "-c".to_string(),
            format!("printf popup > {quoted}"),
        ])
        .expect("display-popup command");

        assert_eq!(status, 0);
        assert_eq!(fs::read_to_string(output).expect("popup output"), "popup");
    }

    #[test]
    fn display_popup_skips_options_before_command() {
        let temp = tempfile::tempdir().expect("tempdir");
        let output = temp.path().join("popup-e.out");
        let output_path = output.display().to_string();
        let quoted = shlex::try_quote(&output_path).expect("quote output");
        let status = display_popup(&[
            "-E".to_string(),
            "-w".to_string(),
            "80".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            format!("printf popup-e > {quoted}"),
        ])
        .expect("display-popup command");

        assert_eq!(status, 0);
        assert_eq!(fs::read_to_string(output).expect("popup output"), "popup-e");
    }

    #[test]
    fn display_popup_accepts_clustered_options_and_explicit_separator() {
        let temp = tempfile::tempdir().expect("tempdir");
        let clustered = temp.path().join("popup-clustered.out");
        let clustered_path = clustered.display().to_string();
        let quoted_clustered = shlex::try_quote(&clustered_path).expect("quote clustered output");
        let status = display_popup(&[
            "-Ew80".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            format!("printf clustered > {quoted_clustered}"),
        ])
        .expect("clustered display-popup command");
        assert_eq!(status, 0);
        assert_eq!(
            fs::read_to_string(clustered).expect("clustered popup output"),
            "clustered"
        );

        let separated = temp.path().join("popup-separator.out");
        let separated_path = separated.display().to_string();
        let quoted_separated = shlex::try_quote(&separated_path).expect("quote separator output");
        let status = display_popup(&[
            "-E".to_string(),
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            format!("printf separator > {quoted_separated}"),
        ])
        .expect("separator display-popup command");
        assert_eq!(status, 0);
        assert_eq!(
            fs::read_to_string(separated).expect("separator popup output"),
            "separator"
        );
    }

    #[test]
    fn wait_for_requires_channel() {
        assert!(
            wait_for(&Vec::new())
                .unwrap_err()
                .to_string()
                .contains("requires a channel")
        );
        assert!(
            wait_for(&args(["-S"]))
                .unwrap_err()
                .to_string()
                .contains("tmux option -S requires a value")
        );
    }

    #[test]
    fn wait_generation_detects_recreated_pruned_channel() {
        assert!(!wait_generation_has_advanced(0, 0));
        assert!(wait_generation_has_advanced(0, 1));
        assert!(wait_generation_has_advanced(7, 8));
        assert!(!wait_generation_has_advanced(7, 7));
        assert!(!wait_generation_has_advanced(7, 0));
        assert!(
            wait_generation_has_advanced(7, 1),
            "a signal that recreates a pruned channel must wake waiters even if the generation resets"
        );
    }

    #[test]
    fn fish_quote_escapes_single_quote_and_backslash() {
        assert_eq!(fish_quote(r"/tmp/with\slash"), r"'/tmp/with\\slash'");
        assert_eq!(fish_quote("/tmp/it's-here"), r"'/tmp/it\'s-here'");
        assert_eq!(fish_quote(r"/tmp/it'\mix"), r"'/tmp/it\'\\mix'");
    }

    #[test]
    fn expands_tmux_formats() {
        let info = SessionInfo {
            id: "id".into(),
            name: "s".into(),
            pane_id: "%1".into(),
            command: "codex --ask".into(),
            cwd: "/tmp".into(),
            agent_name: None,
            created_unix_ms: 0,
            alive: true,
            exit_code: None,
            rows: 24,
            cols: 80,
            parent_pane_id: None,
            parent_session_id: None,
            attached_clients: 0,
            process_id: None,
            process_group_id: None,
            status_theme: None,
        };
        assert_eq!(
            expand_format("#{pane_id} #S #{pane_current_command}", &info),
            "%1 s codex"
        );
        assert_eq!(expand_format("#S:#I #{window_index}", &info), "s:0 0");
        assert_eq!(
            expand_format("#{extended-keys-format}", &info),
            "xterm",
            "Codex/tmux probes expect a concrete extended key format, not a literal placeholder"
        );
    }

    #[test]
    fn rejects_unsupported_tmux_window_targets_with_stable_contract() {
        for target in ["s:#I", "s:1", "s:0.1", ":0", "s:"] {
            let err = reject_unsupported_tmux_window_target(target)
                .expect_err("unsupported window target should fail")
                .to_string();
            assert!(
                err.contains("unsupported tmux window target in lterm compat:"),
                "{err:?}"
            );
            assert!(
                err.contains("lterm supports bare session targets and session:0 only"),
                "{err:?}"
            );
        }
        reject_unsupported_tmux_window_target("s").expect("bare session target is supported");
        reject_unsupported_tmux_window_target("s:0").expect("session:0 target is supported");
        reject_unsupported_tmux_window_target("%1").expect("pane id target is supported");
    }

    #[test]
    fn parses_format_short_flag_forms_without_confusing_values() {
        assert_eq!(
            parse_format(&args(["-F", "#{session_name}"])).as_deref(),
            Some("#{session_name}")
        );
        assert_eq!(
            parse_format(&args(["-F#{session_name}"])).as_deref(),
            Some("#{session_name}")
        );
        assert_eq!(
            parse_format(&args(["-F=#{session_name}"])).as_deref(),
            Some("#{session_name}")
        );
        assert_eq!(
            parse_format(&args(["-aF", "#{session_name}"])).as_deref(),
            Some("#{session_name}")
        );
        assert_eq!(
            parse_format(&args(["-aF#{session_name}"])).as_deref(),
            Some("#{session_name}")
        );
        assert_eq!(
            parse_format(&args(["-aF=#{session_name}"])).as_deref(),
            Some("#{session_name}")
        );
        assert_eq!(
            parse_format(&args(["-tFoo", "-F", "real"])).as_deref(),
            Some("real")
        );
        assert_eq!(parse_format(&args(["--", "-F", "ignored"])), None);
    }

    #[test]
    fn parses_target_without_confusing_format_values() {
        assert_eq!(
            parse_target(&args(["-F", "-t#{session_name}"])).unwrap(),
            None,
            "-F values that look like targets must remain format literals"
        );
        assert_eq!(
            parse_target(&args(["-F", "-t#{session_name}", "-t", "real"]))
                .unwrap()
                .as_deref(),
            Some("real")
        );
        assert_eq!(
            parse_target(&args(["-aF", "-t#{session_name}", "-tfoo"]))
                .unwrap()
                .as_deref(),
            Some("foo")
        );
        assert_eq!(parse_target(&args(["--", "-tfoo"])).unwrap(), None);
        assert_eq!(
            parse_target(&args(["-t", "first", "-t", "second"]))
                .unwrap()
                .as_deref(),
            Some("second"),
            "later target flags override earlier ones before --"
        );
        assert_eq!(
            parse_target_with_value_flags(&args(["-S", "-tformat", "-t", "real"]), &['S'])
                .unwrap()
                .as_deref(),
            Some("real"),
            "value-taking flags must hide target-looking values during the scan"
        );
        assert_eq!(
            parse_target(&args(["-c", "/tmp", "-t", "real"]))
                .unwrap()
                .as_deref(),
            Some("real"),
            "built-in value-taking flags must hide target-looking values during the scan"
        );
        assert_eq!(
            parse_target(&args(["-at", "foo"])).unwrap().as_deref(),
            Some("foo")
        );
        assert_eq!(
            parse_target(&args(["-at", "first", "-t", "second"]))
                .unwrap()
                .as_deref(),
            Some("second"),
            "later targets must override clustered -t values"
        );
        assert_eq!(
            parse_target(&args(["-atfoo"])).unwrap().as_deref(),
            Some("foo")
        );
        assert_eq!(
            parse_target(&args(["-at=foo"])).unwrap().as_deref(),
            Some("foo")
        );
        assert_eq!(
            parse_target(&args(["literal-key", "-t", "payload-target"])).unwrap(),
            None,
            "positional payloads terminate the generic target scan"
        );
        assert!(parse_target(&args(["-t"])).is_err());
        assert!(parse_target(&args(["-t="])).is_err());
    }

    #[test]
    fn parses_split_window_env_assignments_explicitly() {
        let mut env = HashMap::new();
        parse_split_window_env_assignment("FOO=bar".to_string(), &mut env)
            .expect("valid environment assignment");
        parse_split_window_env_assignment("EMPTY=".to_string(), &mut env)
            .expect("empty values are valid tmux -e assignments");

        assert_eq!(env["FOO"], "bar");
        assert_eq!(env["EMPTY"], "");
        assert!(
            parse_split_window_env_assignment("FOO".to_string(), &mut env)
                .unwrap_err()
                .to_string()
                .contains("requires NAME=value")
        );
        assert!(
            parse_split_window_env_assignment("=value".to_string(), &mut env)
                .unwrap_err()
                .to_string()
                .contains("non-empty variable name")
        );
    }

    #[test]
    fn omx_hud_watch_detection_accepts_command_or_env_owner() {
        let mut env = HashMap::new();
        assert!(is_omx_hud_watch_command(
            Some("exec env OMX_TMUX_HUD_OWNER=1 node omx.js hud --watch"),
            &env
        ));

        env.insert("OMX_TMUX_HUD_OWNER".to_string(), "1".to_string());
        assert!(is_omx_hud_watch_command(
            Some("exec node omx.js hud --watch"),
            &env
        ));
        assert!(!is_omx_hud_watch_command(
            Some("exec node omx.js hud"),
            &env
        ));

        env.insert("OMX_TMUX_HUD_OWNER".to_string(), "0".to_string());
        assert!(!is_omx_hud_watch_command(
            Some("exec node omx.js hud --watch"),
            &env
        ));
    }

    #[test]
    fn rejects_missing_new_session_option_values() {
        assert!(
            new_session(&args(["-d", "-s"]))
                .unwrap_err()
                .to_string()
                .contains("tmux option -s requires a value")
        );
        assert!(
            new_session(&args(["-d", "-c"]))
                .unwrap_err()
                .to_string()
                .contains("tmux option -c requires a value")
        );
    }

    #[test]
    fn rejects_missing_rename_session_values_before_rpc() {
        assert!(
            rename_session(&args(["-t"]))
                .unwrap_err()
                .to_string()
                .contains("tmux target flag -t requires a value")
        );
        assert!(
            rename_session(&args(["-t", "old"]))
                .unwrap_err()
                .to_string()
                .contains("tmux rename-session requires a new session name")
        );
        assert!(
            rename_session(&args(["old", "extra"]))
                .unwrap_err()
                .to_string()
                .contains("exactly one new session name")
        );
        assert!(
            rename_session(&args(["-t", "old", "--"]))
                .unwrap_err()
                .to_string()
                .contains("requires a new session name")
        );
    }

    #[test]
    fn rejects_invalid_resize_pane_dimensions() {
        assert!(
            resize_pane(&args(["-x"]))
                .unwrap_err()
                .to_string()
                .contains("resize-pane -x requires a dimension value")
        );
        assert!(
            resize_pane(&args(["-y", "wat"]))
                .unwrap_err()
                .to_string()
                .contains("invalid resize-pane -y dimension value")
        );
        assert!(
            resize_pane(&args(["-x", "0"]))
                .unwrap_err()
                .to_string()
                .contains("resize dimensions must be at least 1")
        );
    }

    #[test]
    fn parses_capture_pane_args_in_one_pass() {
        assert_eq!(
            parse_capture_pane_args(&args(["-p", "-Stop", "-E", "10", "-t", "capture-second"]))
                .unwrap(),
            CapturePaneArgs {
                target: Some("capture-second".into()),
                print: true,
                start: Some(0),
                end: Some(10),
            }
        );
        assert_eq!(
            parse_capture_pane_args(&args(["-p", "-S1", "-E0", "-tcap"])).unwrap(),
            CapturePaneArgs {
                target: Some("cap".into()),
                print: true,
                start: Some(1),
                end: Some(0),
            }
        );
        assert_eq!(
            parse_capture_pane_args(&args(["-t=cap"])).unwrap(),
            CapturePaneArgs {
                target: Some("cap".into()),
                print: false,
                start: None,
                end: None,
            }
        );
        assert_eq!(
            parse_capture_pane_args(&args(["-tS1"])).unwrap(),
            CapturePaneArgs {
                target: Some("S1".into()),
                print: false,
                start: None,
                end: None,
            },
            "attached target values must not be reinterpreted as -S/-E/-p flags"
        );
        assert_eq!(
            parse_capture_pane_args(&args(["-b", "-p", "-t", "cap"])).unwrap(),
            CapturePaneArgs {
                target: Some("cap".into()),
                print: false,
                start: None,
                end: None,
            },
            "-p after -b is a buffer name, not the print flag"
        );
        assert_eq!(
            parse_capture_pane_args(&args(["-t", "first", "-t", "second"]))
                .unwrap()
                .target
                .as_deref(),
            Some("second"),
            "tmux capture-pane lets later target flags override earlier ones"
        );
        assert_eq!(
            parse_capture_pane_args(&args(["-pt", "first", "-t", "second"])).unwrap(),
            CapturePaneArgs {
                target: Some("second".into()),
                print: true,
                start: None,
                end: None,
            },
            "clustered -t values should be consumed before later target overrides"
        );
        assert_eq!(
            parse_capture_pane_args(&args(["--", "-t", "ignored"])).unwrap(),
            CapturePaneArgs::default(),
            "`--` terminates capture-pane option parsing"
        );
        assert!(
            parse_capture_pane_args(&args(["-p", "-E", "-t", "cap"]))
                .unwrap_err()
                .to_string()
                .contains("invalid capture-pane -E line value"),
            "-E must not consume -t as a target when its value is invalid"
        );
    }

    #[test]
    fn parses_buffer_path_after_buffer_options() {
        assert_eq!(
            buffer_path_arg(&args(["-b", "named", "/tmp/input"])).as_deref(),
            Some("/tmp/input")
        );
        assert_eq!(
            buffer_path_arg(&args(["-abnamed", "/tmp/input"])).as_deref(),
            Some("/tmp/input")
        );
        assert_eq!(
            buffer_path_arg(&args(["--", "-literal-path"])).as_deref(),
            Some("-literal-path")
        );
    }

    #[test]
    fn rejects_only_real_filter_short_flags() {
        assert!(reject_filter(&args(["-tfoo"])).is_ok());
        assert!(reject_filter(&args(["-Ffoo"])).is_ok());
        assert!(reject_filter(&args(["-aFfoo"])).is_ok());
        assert!(reject_filter(&args(["--", "-f"])).is_ok());
        assert!(reject_filter(&args(["-s", "-f", "#{session_name}"])).is_err());
        assert!(reject_filter(&args(["-f", "#{session_name}"])).is_err());
        assert!(reject_filter(&args(["-f#{session_name}"])).is_err());
        assert!(reject_filter(&args(["-af", "#{session_name}"])).is_err());
    }

    #[test]
    fn parses_short_flag_clusters_consistently() {
        assert!(has_flag(&args(["-aF#{session_name}"]), "-a"));
        assert!(!has_flag(&args(["-F#{session_name}"]), "-a"));
        assert_eq!(
            flag_arg_width("-aF", &args(["-aF", "#{session_name}"]), 0),
            2
        );
        assert_eq!(
            flag_arg_width("-aF#{session_name}", &args(["-aF#{session_name}"]), 0),
            1
        );
        assert_eq!(flag_arg_width("-tfoo", &args(["-tfoo"]), 0), 1);
        assert_eq!(flag_arg_width("-t", &args(["-t", "foo"]), 0), 2);
        assert_eq!(flag_arg_width("-s", &args(["-s", "-F", "format"]), 0), 1);
    }

    #[test]
    fn prune_wait_generations_removes_stale_channels_and_preserves_active() {
        let mut store = CompatStore::default();
        let stale = "stale-channel".to_string();
        let active = "active-channel".to_string();
        let now = now_unix_secs();
        store.wait_generations.insert(stale.clone(), 7);
        store.wait_generations.insert(active.clone(), 8);
        store.wait_generation_touched_secs.insert(
            stale.clone(),
            now.saturating_sub(WAIT_GENERATION_RETENTION_SECS + 1),
        );
        store.wait_generation_touched_secs.insert(
            active.clone(),
            now.saturating_sub(WAIT_GENERATION_RETENTION_SECS + 1),
        );

        prune_wait_generations(&mut store, Some(&active));

        assert!(!store.wait_generations.contains_key(&stale));
        assert!(!store.wait_generation_touched_secs.contains_key(&stale));
        assert_eq!(store.wait_generations.get(&active), Some(&8));
    }

    #[test]
    fn prune_wait_generations_timestamps_legacy_entries() {
        let mut store = CompatStore::default();
        store
            .wait_generations
            .insert("legacy-channel".to_string(), 1);

        prune_wait_generations(&mut store, None);

        assert_eq!(store.wait_generations.get("legacy-channel"), Some(&1));
        assert!(
            store
                .wait_generation_touched_secs
                .contains_key("legacy-channel")
        );
    }

    #[test]
    fn prune_wait_generations_removes_touched_only_channels() {
        let mut store = CompatStore::default();
        store
            .wait_generation_touched_secs
            .insert("unsignaled-channel".to_string(), now_unix_secs());

        prune_wait_generations(&mut store, Some("unsignaled-channel"));

        assert!(
            store.wait_generation_touched_secs.is_empty(),
            "waiters on never-signaled channels must not grow touched metadata"
        );
        assert!(store.wait_generations.is_empty());
    }

    #[test]
    fn cmux_surface_ref_prefers_focused_surface_over_caller() {
        let value = serde_json::json!({
            "caller": {
                "surface_ref": "surface:caller"
            },
            "focused": {
                "surface_ref": "surface:focused"
            }
        });

        assert_eq!(
            find_cmux_surface_ref(&value).as_deref(),
            Some("surface:focused")
        );
    }

    #[test]
    fn cmux_surface_ref_accepts_legacy_surface_id() {
        let value = serde_json::json!({
            "surface_id": "legacy-surface-id"
        });

        assert_eq!(
            find_cmux_surface_ref(&value).as_deref(),
            Some("legacy-surface-id")
        );
    }

    #[test]
    fn cmux_surface_ref_does_not_fall_back_to_caller_when_focused_lacks_surface() {
        let value = serde_json::json!({
            "caller": {
                "surface_ref": "surface:caller"
            },
            "focused": {
                "workspace_ref": "workspace:focused"
            }
        });

        assert_eq!(find_cmux_surface_ref(&value), None);
    }

    #[test]
    fn cmux_surface_ref_does_not_use_caller_when_focused_is_absent() {
        let value = serde_json::json!({
            "caller": {
                "surface_ref": "surface:caller"
            }
        });

        assert_eq!(find_cmux_surface_ref(&value), None);
    }

    #[test]
    fn cmux_managed_attach_surface_prefers_caller_over_focused() {
        let value = serde_json::json!({
            "caller": {
                "surface_ref": "surface:caller",
                "workspace_ref": "workspace:caller",
                "window_ref": "window:caller"
            },
            "focused": {
                "surface_ref": "surface:focused",
                "workspace_ref": "workspace:focused",
                "window_ref": "window:focused"
            }
        });

        assert_eq!(
            find_cmux_managed_attach_surface_ref(&value).as_deref(),
            Some("surface:caller")
        );
        let context = find_cmux_managed_attach_surface_context(&value)
            .expect("managed attach caller context");
        assert_eq!(context.workspace_ref.as_deref(), Some("workspace:caller"));
        assert_eq!(context.window_ref.as_deref(), Some("window:caller"));
    }

    #[test]
    fn cmux_managed_attach_surface_rejects_focused_without_caller() {
        let value = serde_json::json!({
            "focused": {
                "surface_ref": "surface:focused"
            }
        });

        assert_eq!(find_cmux_managed_attach_surface_ref(&value), None);
    }

    #[test]
    fn cmux_managed_attach_surface_skips_malformed_explicit_candidate() {
        let value = serde_json::json!({
            "caller": {
                "surface_ref": "--not-safe"
            },
            "current": {
                "surface_ref": "surface:current",
                "workspace_ref": "workspace:current",
                "window_ref": "window:current"
            }
        });

        let context = find_cmux_managed_attach_surface_context(&value)
            .expect("valid current context should be used after malformed caller");
        assert_eq!(context.surface_ref, "surface:current");
        assert_eq!(context.workspace_ref.as_deref(), Some("workspace:current"));
        assert_eq!(context.window_ref.as_deref(), Some("window:current"));
    }

    #[test]
    fn cmux_managed_attach_surface_rejects_top_level_legacy_identity() {
        let value = serde_json::json!({
            "surface_ref": "surface:ambiguous",
            "workspace_ref": "workspace:1"
        });

        assert_eq!(find_cmux_managed_attach_surface_ref(&value), None);
    }

    #[test]
    fn cmux_surface_ref_keeps_legacy_whole_document_lookup_without_caller_schema() {
        let value = serde_json::json!({
            "pane": {
                "surface_id": "legacy-surface-id"
            }
        });

        assert_eq!(
            find_cmux_surface_ref(&value).as_deref(),
            Some("legacy-surface-id")
        );
    }

    #[test]
    fn cmux_surface_context_keeps_focused_workspace_and_window_refs() {
        let value = serde_json::json!({
            "caller": {
                "surface_ref": "surface:stale",
                "workspace_ref": "workspace:stale",
                "window_ref": "window:stale"
            },
            "focused": {
                "surface_ref": "surface:focused",
                "workspace_ref": "workspace:focused",
                "window_ref": "window:focused"
            }
        });

        let context = find_cmux_surface_context(&value).expect("focused surface context");
        assert_eq!(context.surface_ref, "surface:focused");
        assert_eq!(context.workspace_ref.as_deref(), Some("workspace:focused"));
        assert_eq!(context.window_ref.as_deref(), Some("window:focused"));
    }

    /// C4-4: stored split-time 컨텍스트가 있으면 `cmux_status_identity`는 그것을 반환하고
    /// `cmux identify`를 호출하지 않는다(설계 §4.2 AC: identify 미호출). 테스트는 cmux 바이너리
    /// 없이도 통과해야 하며, stored가 우선되므로 식별 서브프로세스가 돌지 않음이 보장된다.
    #[test]
    fn cmux_status_identity_prefers_stored_context_without_identify() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["LTERM_DATA_DIR"]);
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path().join("data");

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_DATA_DIR", &data_dir);
        }

        let stored = CmuxSurfaceContext {
            surface_ref: "surface:uuid-stored".to_string(),
            workspace_ref: Some("workspace:uuid-stored".to_string()),
            window_ref: Some("window:uuid-stored".to_string()),
        };
        let info = SessionInfo {
            id: "session-uuid".to_string(),
            name: "codex".to_string(),
            pane_id: "%42".to_string(),
            parent_pane_id: None,
            parent_session_id: None,
            command: "codex".to_string(),
            cwd: "/tmp".to_string(),
            agent_name: Some("codex".to_string()),
            created_unix_ms: 0,
            alive: true,
            exit_code: None,
            rows: 24,
            cols: 80,
            attached_clients: 0,
            process_id: None,
            process_group_id: None,
            status_theme: None,
        };
        remember_pane(&info, Some(&stored)).expect("seed stored cmux surface");

        let resolved = cmux_status_identity("%42").expect("stored context should resolve");
        assert_eq!(resolved, stored);
    }

    /// C4-4: identify 폴백은 `find_cmux_surface_context`의 stale-caller 거부를 재사용한다 —
    /// caller만 있고 focused가 없으면 `None`(설계 §4.2 AC: stale-caller만 → None).
    #[test]
    fn cmux_status_identity_fallback_rejects_caller_only_payload() {
        let caller_only = serde_json::json!({
            "caller": {
                "surface_ref": "surface:stale-caller",
                "workspace_ref": "workspace:stale-caller"
            }
        });
        assert_eq!(find_cmux_surface_context(&caller_only), None);
    }

    /// C4-4: status 식별 폴백은 `--id-format uuids`로 UUID 우선 캡처한다(설계 §4.2 AC,
    /// positional ref 드리프트 R1 방지).
    #[test]
    fn cmux_status_identity_requests_uuid_id_format() {
        let args = cmux_identify_args(Some("uuids"));
        assert_eq!(args, vec!["identify", "--json", "--id-format", "uuids"]);
        // None이면 기존 동작(추가 인자 없음).
        assert_eq!(cmux_identify_args(None), vec!["identify", "--json"]);
    }

    #[test]
    fn cmux_surface_context_rejects_invalid_json_refs() {
        let invalid_surface = serde_json::json!({
            "focused": {
                "surface_ref": "surface:bad ref",
                "workspace_ref": "workspace:focused"
            }
        });
        assert_eq!(find_cmux_surface_context(&invalid_surface), None);

        let invalid_optional_refs = serde_json::json!({
            "focused": {
                "surface_ref": "surface:focused",
                "workspace_ref": "workspace:bad ref",
                "window_ref": "-window-option"
            }
        });
        let context =
            find_cmux_surface_context(&invalid_optional_refs).expect("valid surface remains");
        assert_eq!(context.surface_ref, "surface:focused");
        assert_eq!(context.workspace_ref, None);
        assert_eq!(context.window_ref, None);
    }

    #[test]
    fn cmux_new_split_output_reports_ok_surface_ref() {
        assert_eq!(
            parse_cmux_new_split_surface(b"OK surface:42 workspace:1\n").as_deref(),
            Some("surface:42")
        );
    }

    #[test]
    fn cmux_new_split_output_reports_workspace_and_window_context() {
        let context = parse_cmux_new_split_surface_context(
            b"note surface:stale\nOK surface:created workspace:focused window:main\n",
        )
        .expect("new split surface context");

        assert_eq!(context.surface_ref, "surface:created");
        assert_eq!(context.workspace_ref.as_deref(), Some("workspace:focused"));
        assert_eq!(context.window_ref.as_deref(), Some("window:main"));
    }

    #[test]
    fn cmux_new_split_output_ignores_non_ok_surface_mentions() {
        assert_eq!(
            parse_cmux_new_split_surface(b"note previous surface:9\nOK surface:42 workspace:1\n")
                .as_deref(),
            Some("surface:42")
        );
        assert_eq!(parse_cmux_new_split_surface(b"note surface:9\n"), None);
    }

    #[test]
    fn cmux_new_split_output_rejects_empty_or_decorated_surface_refs() {
        assert_eq!(
            parse_cmux_new_split_surface(b"OK surface: workspace:1\n"),
            None
        );
        assert_eq!(
            parse_cmux_new_split_surface(b"OK (surface:42) workspace:1\n"),
            None
        );
    }

    #[test]
    fn cmux_split_targets_identified_focused_surface_not_stale_env_surface() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["PATH", "CMUX_SURFACE_ID", "CMUX_WORKSPACE_ID"]);
        let temp = tempfile::tempdir().expect("tempdir");
        let cmux_path = temp.path().join("cmux");
        let log_path = temp.path().join("cmux.log");
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> {log}
if [ "$1" = "identify" ] && [ "$2" = "--json" ]; then
  printf '%s\n' '{{"caller":{{"surface_ref":"surface:stale","workspace_ref":"workspace:stale","window_ref":"window:stale"}},"focused":{{"surface_ref":"surface:focused","workspace_ref":"workspace:focused","window_ref":"window:focused"}}}}'
  exit 0
fi
if [ "$1" = "new-split" ]; then
  if [ "$*" != "new-split down --surface surface:focused --workspace workspace:focused --window window:focused --focus true" ]; then
    printf 'unexpected args: %s\n' "$*" >&2
    exit 64
  fi
  printf '%s\n' 'OK surface:created workspace:focused'
  exit 0
fi
printf 'unexpected command: %s\n' "$*" >&2
exit 65
"#,
            log = shlex::try_quote(&log_path.display().to_string()).expect("quote log path")
        );
        fs::write(&cmux_path, script).expect("write fake cmux");
        let mut permissions = fs::metadata(&cmux_path)
            .expect("fake cmux metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&cmux_path, permissions).expect("chmod fake cmux");

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var(
                "PATH",
                format!(
                    "{}:{}",
                    temp.path().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
            std::env::set_var("CMUX_SURFACE_ID", "surface:stale");
            std::env::set_var("CMUX_WORKSPACE_ID", "workspace:stale");
        }

        let surface = open_cmux_split("down")
            .expect("open cmux split")
            .expect("surface context");
        assert_eq!(surface.surface_ref, "surface:created");
        assert_eq!(surface.workspace_ref.as_deref(), Some("workspace:focused"));
        assert_eq!(surface.window_ref.as_deref(), Some("window:focused"));
        let log = fs::read_to_string(&log_path).expect("read fake cmux log");
        assert!(log.contains("identify --json"));
        assert!(
            log.contains(
                "new-split down --surface surface:focused --workspace workspace:focused --window window:focused --focus true"
            ),
            "{log}"
        );
        assert!(!log.contains("new-split down --surface surface:stale"));
    }

    #[test]
    fn cmux_stderr_preview_strips_terminal_controls_and_truncates() {
        let mut stderr = b"\x1b]52;c;secret\x07CMUX_FAIL\nNEXT ".to_vec();
        stderr.extend(std::iter::repeat_n(b'x', 600));
        let preview = cmux_stderr_preview(&stderr).expect("stderr preview");

        assert!(preview.starts_with("CMUX_FAIL NEXT "));
        assert!(preview.ends_with('…'));
        assert!(!preview.contains('\x1b'));
        assert!(!preview.contains("secret"));
        assert!(preview.chars().count() <= 513);
    }

    #[test]
    fn child_lterm_executable_prefers_safe_absolute_override() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["LTERM_BIN"]);
        let temp = tempfile::tempdir().expect("tempdir");
        let lterm = temp.path().join("lterm");
        fs::write(&lterm, b"#!/bin/sh\n").expect("write fake lterm");
        let mut permissions = fs::metadata(&lterm)
            .expect("fake lterm metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&lterm, permissions).expect("chmod fake lterm");

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_BIN", &lterm);
        }

        assert_eq!(child_lterm_executable(), lterm.display().to_string());
    }

    #[test]
    fn child_lterm_executable_ignores_unsafe_override() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["LTERM_BIN"]);

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_BIN", "relative/lterm");
        }

        assert_ne!(child_lterm_executable(), "relative/lterm");
    }

    #[test]
    fn managed_attach_claim_serializes_duplicate_owners() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["LTERM_DATA_DIR"]);
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path().join("data");

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_DATA_DIR", &data_dir);
        }

        let owner = CmuxSurfaceContext {
            surface_ref: "surface:owner".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };
        let duplicate = CmuxSurfaceContext {
            surface_ref: "surface:duplicate".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };

        let first = claim_managed_attach("%9", "token-owner", &owner).expect("first claim");
        assert!(first.proceed);

        let second =
            claim_managed_attach("%9", "token-duplicate", &duplicate).expect("second claim");
        assert!(!second.proceed);
        assert_eq!(second.owner_surface_id.as_deref(), Some("surface:owner"));

        let same_surface =
            claim_managed_attach("%9", "token-restart", &owner).expect("same-surface claim");
        assert!(
            same_surface.proceed,
            "same cmux surface may replace a stale restarted attach owner"
        );
    }

    #[test]
    fn managed_attach_claim_replaces_dead_owner_pid() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["LTERM_DATA_DIR"]);
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path().join("data");

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_DATA_DIR", &data_dir);
        }

        let owner = CmuxSurfaceContext {
            surface_ref: "surface:dead-owner".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };
        let replacement = CmuxSurfaceContext {
            surface_ref: "surface:replacement".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };

        let claim = claim_managed_attach("%12", "token-owner", &owner).expect("owner claim");
        assert!(claim.proceed);
        update_store(|store| {
            let lease = store
                .managed_attaches
                .get_mut("%12")
                .expect("owner lease should exist");
            lease.pid = 9_999_999;
            Ok(())
        })
        .expect("mark lease dead");

        let replacement_claim = claim_managed_attach("%12", "token-replacement", &replacement)
            .expect("replacement claim");
        assert!(
            replacement_claim.proceed,
            "dead owner pid should not suppress a legitimate replacement attach"
        );
    }

    #[test]
    fn managed_attach_claim_replaces_pid_reuse_identity_mismatch() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["LTERM_DATA_DIR"]);
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path().join("data");

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_DATA_DIR", &data_dir);
        }

        let owner = CmuxSurfaceContext {
            surface_ref: "surface:owner".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };
        let replacement = CmuxSurfaceContext {
            surface_ref: "surface:replacement".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };

        let claim = claim_managed_attach("%13", "token-owner", &owner).expect("owner claim");
        assert!(claim.proceed);
        update_store(|store| {
            let lease = store
                .managed_attaches
                .get_mut("%13")
                .expect("owner lease should exist");
            lease.process_start_id = Some("reused-pid-different-process".to_string());
            Ok(())
        })
        .expect("mark lease identity mismatch");

        let replacement_claim = claim_managed_attach("%13", "token-replacement", &replacement)
            .expect("replacement claim");
        assert!(
            replacement_claim.proceed,
            "a live reused pid with mismatched start identity must not suppress replacement"
        );
    }

    #[test]
    fn managed_attach_claim_suppresses_fresh_identityless_live_owner() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["LTERM_DATA_DIR"]);
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path().join("data");

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_DATA_DIR", &data_dir);
        }

        let owner = CmuxSurfaceContext {
            surface_ref: "surface:owner".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };
        let duplicate = CmuxSurfaceContext {
            surface_ref: "surface:duplicate".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };

        let claim = claim_managed_attach("%15", "token-owner", &owner).expect("owner claim");
        assert!(claim.proceed);
        update_store(|store| {
            let lease = store
                .managed_attaches
                .get_mut("%15")
                .expect("owner lease should exist");
            lease.process_start_id = None;
            lease.updated_secs = now_unix_secs();
            Ok(())
        })
        .expect("simulate legacy identityless owner");

        let duplicate_claim =
            claim_managed_attach("%15", "token-duplicate", &duplicate).expect("duplicate claim");
        assert!(
            !duplicate_claim.proceed,
            "fresh identityless but live owner leases must suppress duplicate attaches"
        );
        assert_eq!(
            duplicate_claim.owner_surface_id.as_deref(),
            Some("surface:owner")
        );
    }

    #[test]
    fn managed_attach_claim_replaces_stale_identityless_live_owner() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["LTERM_DATA_DIR"]);
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path().join("data");

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_DATA_DIR", &data_dir);
        }

        let owner = CmuxSurfaceContext {
            surface_ref: "surface:owner".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };
        let replacement = CmuxSurfaceContext {
            surface_ref: "surface:replacement".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };

        let claim = claim_managed_attach("%16", "token-owner", &owner).expect("owner claim");
        assert!(claim.proceed);
        update_store(|store| {
            let lease = store
                .managed_attaches
                .get_mut("%16")
                .expect("owner lease should exist");
            lease.process_start_id = None;
            lease.updated_secs = 1;
            Ok(())
        })
        .expect("simulate stale legacy identityless owner");

        let replacement_claim = claim_managed_attach("%16", "token-replacement", &replacement)
            .expect("replacement claim");
        assert!(
            replacement_claim.proceed,
            "stale identityless owner leases must expire instead of blocking future attaches"
        );
    }

    #[test]
    fn managed_attach_claim_serializes_near_simultaneous_threads() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["LTERM_DATA_DIR"]);
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path().join("data");

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_DATA_DIR", &data_dir);
        }

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let claims = [("token-a", "surface:a"), ("token-b", "surface:b")]
            .into_iter()
            .map(|(token, surface)| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let current = CmuxSurfaceContext {
                        surface_ref: surface.to_string(),
                        workspace_ref: Some("workspace:1".to_string()),
                        window_ref: None,
                    };
                    barrier.wait();
                    claim_managed_attach("%10", token, &current)
                        .expect("concurrent managed attach claim")
                        .proceed
                })
            })
            .collect::<Vec<_>>();

        let proceeded = claims
            .into_iter()
            .map(|claim| claim.join().expect("claim thread"))
            .filter(|proceeded| *proceeded)
            .count();
        assert_eq!(
            proceeded, 1,
            "exactly one near-simultaneous managed attach may own a pane"
        );
    }

    #[test]
    fn managed_attach_prune_keeps_live_owner_across_stale_timestamp() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["LTERM_DATA_DIR"]);
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path().join("data");

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_DATA_DIR", &data_dir);
        }

        let owner = CmuxSurfaceContext {
            surface_ref: "surface:owner".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };
        let duplicate = CmuxSurfaceContext {
            surface_ref: "surface:duplicate".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };

        let claim = claim_managed_attach("%14", "token-owner", &owner).expect("claim owner");
        assert!(claim.proceed);
        update_store(|store| {
            let lease = store
                .managed_attaches
                .get_mut("%14")
                .expect("owner lease should exist");
            lease.updated_secs = 1;
            Ok(())
        })
        .expect("age live owner lease");

        let duplicate_claim =
            claim_managed_attach("%14", "token-duplicate", &duplicate).expect("duplicate claim");
        assert!(
            !duplicate_claim.proceed,
            "a live owner with matching process identity must survive stale timestamps after sleep/wake"
        );
    }

    #[test]
    fn managed_attach_renewal_keeps_owner_from_expiring() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["LTERM_DATA_DIR"]);
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path().join("data");

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_DATA_DIR", &data_dir);
        }

        let owner = CmuxSurfaceContext {
            surface_ref: "surface:owner".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };
        let duplicate = CmuxSurfaceContext {
            surface_ref: "surface:duplicate".to_string(),
            workspace_ref: Some("workspace:1".to_string()),
            window_ref: None,
        };

        let claim = claim_managed_attach("%11", "token-owner", &owner).expect("claim owner");
        assert!(claim.proceed);
        assert!(renew_managed_attach("%11", "token-owner").expect("renew owner"));

        let duplicate_claim =
            claim_managed_attach("%11", "token-duplicate", &duplicate).expect("duplicate claim");
        assert!(
            !duplicate_claim.proceed,
            "renewed owner lease must keep suppressing duplicate attaches"
        );
        assert_eq!(
            duplicate_claim.owner_surface_id.as_deref(),
            Some("surface:owner")
        );
    }

    #[test]
    fn compat_store_loads_without_managed_attach_field() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["LTERM_DATA_DIR"]);
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path().join("data");
        fs::create_dir_all(&data_dir).expect("create data dir");
        let mut permissions = fs::metadata(&data_dir)
            .expect("data dir metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&data_dir, permissions).expect("chmod data dir");
        fs::write(
            data_dir.join("tmux-compat-store.json"),
            br#"{
              "panes": {},
              "wait_generations": {},
              "wait_generation_touched_secs": {}
            }"#,
        )
        .expect("write old store");

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_DATA_DIR", &data_dir);
        }

        let store = load_store().expect("old store should load with serde defaults");
        assert!(store.managed_attaches.is_empty());
    }

    struct InterruptedOnceReader {
        interrupted: bool,
        cursor: std::io::Cursor<Vec<u8>>,
    }

    impl InterruptedOnceReader {
        fn new(bytes: &[u8]) -> Self {
            Self {
                interrupted: false,
                cursor: std::io::Cursor::new(bytes.to_vec()),
            }
        }
    }

    impl Read for InterruptedOnceReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            self.cursor.read(buf)
        }
    }

    #[test]
    fn limited_output_reader_retries_interrupted_reads() {
        let output =
            read_limited_output(InterruptedOnceReader::new(b"cmux output"), 32).expect("read");

        assert_eq!(output.bytes, b"cmux output");
        assert!(!output.truncated);
    }

    #[test]
    fn limited_output_reader_reports_truncation_while_draining() {
        let output =
            read_limited_output(std::io::Cursor::new(b"abcdef".to_vec()), 3).expect("read");

        assert_eq!(output.bytes, b"abc");
        assert!(output.truncated);
    }

    fn args(values: impl IntoIterator<Item = &'static str>) -> Vec<String> {
        values.into_iter().map(String::from).collect()
    }
}
