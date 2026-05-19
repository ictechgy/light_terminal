use crate::client;
use crate::client::AttachStdinEof;
use crate::paths;
use crate::protocol::SessionInfo;
use crate::sanitize;
use crate::server;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
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
        "-V" | "version" => {
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
        "select-pane" | "selectp" => Ok(0),
        "select-layout" | "selectl" => Ok(0),
        "set-option" | "set" | "setw" | "set-window-option" => Ok(0),
        "show-options"
        | "show"
        | "show-option"
        | "showw"
        | "show-window-option"
        | "show-window-options" => show_option(rest),
        "display-popup" | "popup" => display_popup(rest),
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
            bail!("unsupported tmux command in lterm compat: {command} {args}")
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
            flag if flag.starts_with('-') => i += flag_arg_width(flag, args, i),
            _ => {
                command.extend_from_slice(&args[i..]);
                break;
            }
        }
    }
    let command = tmux_shell_command(&command)?;
    let info = client::new_session(name, command, cwd, HashMap::new(), None, true)?;
    remember_pane(&info, None)?;
    if detached {
        Ok(0)
    } else {
        client::attach(&info.name, true, AttachStdinEof::KeepAttached)?;
        Ok(0)
    }
}

fn attach_session(args: &[String]) -> Result<i32> {
    let target = parse_target(args)?.unwrap_or_else(default_target);
    client::attach(&target, true, AttachStdinEof::Detach)?;
    Ok(0)
}

fn has_session(args: &[String]) -> Result<i32> {
    let target = parse_target(args)?.unwrap_or_else(default_target);
    match client::info(&target) {
        Ok(_) => Ok(0),
        Err(_) => Ok(1),
    }
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
    let mut pane = client::info(target)?;
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

fn kill_session(args: &[String]) -> Result<i32> {
    let target = parse_target(args)?.unwrap_or_else(default_target);
    client::kill(&target)?;
    Ok(0)
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
            "--" => {
                command.extend_from_slice(&args[i + 1..]);
                break;
            }
            flag if flag.starts_with('-') => {
                if has_flag_in_arg(flag, 'h') {
                    direction = "right";
                }
                if has_flag_in_arg(flag, 'v') {
                    direction = "down";
                }
                if has_flag_in_arg(flag, 'd') {
                    detached = true;
                }
                if has_flag_in_arg(flag, 'P') {
                    print = true;
                }
                if let Some((_, value)) = short_cluster_flag_value(flag, 'F', args, i) {
                    format = value_for_option(value.or_else(|| args.get(i + 1).cloned()), "-F")?;
                }
                if let Some((_, value)) = short_cluster_flag_value(flag, 't', args, i) {
                    target = target_value(value.or_else(|| args.get(i + 1).cloned()), "-t")?;
                }
                if let Some((_, value)) = short_cluster_flag_value(flag, 'c', args, i) {
                    cwd = Some(value_for_option(
                        value.or_else(|| args.get(i + 1).cloned()),
                        "-c",
                    )?);
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
    let info = client::new_session(None, command, cwd, HashMap::new(), None, true)?;

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
    reject_filter(args)?;
    let format = parse_format(args).unwrap_or_else(|| "#{pane_id}".to_string());
    if let Some(target) = parse_target(args)? {
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
    let parsed = parse_capture_pane_args(args)?;
    let target = parsed.target.unwrap_or_else(default_target);
    let text = client::capture_range(&target, parsed.start, parsed.end)?;
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
                i += 2;
            }
            "--" => {
                keys.extend_from_slice(&args[i + 1..]);
                break;
            }
            flag if flag.starts_with('-') => {
                i += flag_arg_width_with_extra(flag, args, i, VALUE_FLAGS)
            }
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
    let target = parse_target(args)?.unwrap_or_else(default_target);
    client::kill(&target)?;
    Ok(0)
}

fn resize_pane(args: &[String]) -> Result<i32> {
    let target = parse_target(args)?.unwrap_or_else(default_target);
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
        (Some(rows), Some(cols)) => client::resize(&target, rows, cols, None)?,
        (Some(rows), None) => {
            let info = client::info(&target)?;
            client::resize(&target, rows, info.cols, None)?;
        }
        (None, Some(cols)) => {
            let info = client::info(&target)?;
            client::resize(&target, info.rows, cols, None)?;
        }
        (None, None) => {}
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

fn value_for_option(value: Option<String>, flag: &str) -> Result<String> {
    value.with_context(|| format!("tmux option {flag} requires a value"))
}

fn has_flag_in_arg(arg: &str, needle: char) -> bool {
    has_flag_in_arg_with_value_flags(arg, needle, &[])
}

fn has_flag_in_arg_with_value_flags(arg: &str, needle: char, extra_value_flags: &[char]) -> bool {
    let Some(cluster) = short_cluster(arg) else {
        return false;
    };
    for (pos, flag) in cluster.char_indices() {
        if flag == needle {
            return true;
        }
        if value_for_short_flag_with_extra(cluster, pos, flag, &[], 0, extra_value_flags).is_some()
            || ((is_value_taking_short_flag(flag) || extra_value_flags.contains(&flag))
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
    if let Some(cluster) = short_cluster(flag) {
        for (pos, short_flag) in cluster.char_indices() {
            if is_value_taking_short_flag(short_flag) || extra_value_flags.contains(&short_flag) {
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
    let cluster = short_cluster(arg)?;
    for (pos, flag) in cluster.char_indices() {
        let value = value_for_short_flag_with_extra(cluster, pos, flag, args, i, extra_value_flags);
        if flag == needle {
            return Some((pos, value));
        }
        if value.is_some()
            || ((is_value_taking_short_flag(flag) || extra_value_flags.contains(&flag))
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
    if !is_value_taking_short_flag(flag) && !extra_value_flags.contains(&flag) {
        return None;
    }
    let rest = &cluster[pos + flag.len_utf8()..];
    if rest.is_empty() {
        return args.get(i + 1).cloned();
    }
    Some(rest.strip_prefix('=').unwrap_or(rest).to_string())
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
        "select-layout" | "select-pane" | "set-environment" | "set-option"
        | "set-window-option" | "show-environment" => "noop",
        "attach-session" | "capture-pane" | "has-session" | "kill-pane" | "kill-session"
        | "list-commands" | "list-sessions" | "rename-session" | "send-keys" => "full",
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
    ("rename-session", Some("rename"), &[]),
    ("resize-pane", Some("resizep"), &[]),
    ("save-buffer", Some("saveb"), &[]),
    ("select-layout", Some("selectl"), &[]),
    ("select-pane", Some("selectp"), &[]),
    ("send-keys", Some("send"), &[]),
    ("set-environment", Some("setenv"), &[]),
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
        "rename-session" => "[-t target-session] new-name",
        "resize-pane" => "[-x width] [-y height] [-t target-pane]",
        "save-buffer" => "path",
        "select-layout" => "[-t target-pane] [layout-name]",
        "select-pane" => "[-t target-pane]",
        "send-keys" => "[-l] [-t target-pane] [key ...]",
        "set-environment" => "[-t target-session] variable [value]",
        "set-option" => "[-t target-pane] option [value]",
        "set-window-option" => "[-t target-window] option [value]",
        "show-environment" => "[-t target-session] [variable]",
        "show-options" => "[-t target-pane] [option]",
        "show-window-options" => "[-t target-window] [option]",
        "split-window" => {
            "[-dhvP] [-F format] [-c start-directory] [-t target-pane] [shell-command]"
        }
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

    fn args(values: impl IntoIterator<Item = &'static str>) -> Vec<String> {
        values.into_iter().map(String::from).collect()
    }
}
