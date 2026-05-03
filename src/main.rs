mod client;
mod paths;
mod protocol;
mod sanitize;
mod server;
mod tmux_compat;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use client::AttachStdinEof;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write;
use std::process::Command;

#[derive(Debug, Parser)]
#[command(
    name = "lterm",
    version,
    about = "Light Terminal: a lightweight tmux-compatible session daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run the background PTY session daemon.
    Daemon,
    /// Create a persistent session and attach to it.
    New {
        #[arg(short, long)]
        name: Option<String>,
        #[arg(short = 'c', long)]
        cwd: Option<String>,
        #[arg(long)]
        tmux: bool,
        /// Create the session without attaching to it.
        #[arg(short = 'd', long)]
        detach: bool,
        /// Disable the blue lterm status bar while attached.
        #[arg(long)]
        no_status: bool,
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Create a tmux-shimmed session and attach to it.
    Run {
        #[arg(short, long)]
        name: Option<String>,
        #[arg(short = 'c', long)]
        cwd: Option<String>,
        #[arg(long, default_value_t = true)]
        tmux: bool,
        /// Disable the blue lterm status bar while attached.
        #[arg(long)]
        no_status: bool,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Attach to a persistent session or pane.
    #[command(visible_alias = "a")]
    Attach {
        #[arg(default_value = "%0")]
        target: String,
        /// Disable the blue lterm status bar while attached.
        #[arg(long)]
        no_status: bool,
    },
    /// Attach to a session, creating it first when missing.
    AttachOrNew {
        #[arg(default_value = "main")]
        target: String,
        /// Disable the blue lterm status bar while attached.
        #[arg(long)]
        no_status: bool,
    },
    /// List sessions.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
        /// Include child panes created from inside another lterm session.
        #[arg(long, conflicts_with = "children")]
        all: bool,
        /// Show only child panes created from inside another lterm session.
        #[arg(long, conflicts_with = "all")]
        children: bool,
    },
    /// Show child process trees for lterm sessions.
    Ps {
        target: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Kill a session or pane.
    Kill { target: String },
    /// Send text to a session or pane.
    Send {
        target: String,
        text: String,
        #[arg(long)]
        enter: bool,
    },
    /// Capture scrollback from a session or pane.
    Capture {
        target: String,
        #[arg(short = 'S', long, allow_hyphen_values = true)]
        start: Option<i32>,
    },
    /// Stop the daemon and all sessions.
    Shutdown,
    /// Install the tmux compatibility shim and print the shim directory.
    InstallShim,
    /// Print shell exports for tmux compatibility.
    Env,
    /// tmux-compatible command surface used by the shim.
    TmuxCompat {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Send a cmux-friendly notification.
    Notify {
        #[arg(long)]
        title: String,
        #[arg(long)]
        subtitle: Option<String>,
        #[arg(long, default_value = "")]
        body: String,
    },
    /// Run Oh My Codex inside a tmux-compatible lterm session.
    Omx {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Oh My Claude inside a tmux-compatible lterm session.
    Omc {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Attach to lterm on a remote host over SSH. Requires lterm installed remotely.
    Ssh {
        host: String,
        #[arg(default_value = "main")]
        target: String,
        #[arg(last = true)]
        ssh_args: Vec<String>,
    },
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse_from(expand_attach_short_flag(std::env::args_os()));
    match cli.command {
        Commands::Daemon => server::serve_forever(),
        Commands::New {
            name,
            cwd,
            tmux,
            detach,
            no_status,
            command,
        } => {
            if tmux {
                tmux_compat::ensure_shim()?;
            }
            let command = normalize_command(command)?;
            let info = client::new_session(name, command, cwd, HashMap::new(), tmux)?;
            if detach {
                println!("{}\t{}\t{}", info.name, info.pane_id, info.command);
                Ok(())
            } else {
                client::attach(&info.pane_id, !no_status, AttachStdinEof::KeepAttached)
            }
        }
        Commands::Run {
            name,
            cwd,
            tmux,
            no_status,
            command,
        } => {
            if tmux {
                tmux_compat::ensure_shim()?;
            }
            let command = normalize_command(command)?.context("run requires a command")?;
            let info = client::new_session(name, Some(command), cwd, HashMap::new(), tmux)?;
            client::attach(&info.pane_id, !no_status, AttachStdinEof::KeepAttached)
        }
        Commands::Attach { target, no_status } => {
            client::attach(&target, !no_status, AttachStdinEof::Detach)
        }
        Commands::AttachOrNew { target, no_status } => client::attach(
            &client::attach_or_new(&target)?.pane_id,
            !no_status,
            AttachStdinEof::Detach,
        ),
        Commands::List {
            json,
            all,
            children,
        } => {
            let sessions = filter_list_sessions(
                collapse_aliases(client::list_sessions()?),
                ListScope::from_flags(all, children),
            );
            if json {
                println!("{}", client::json_pretty(&sessions));
            } else {
                for s in sessions {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        sanitize::terminal_text(&s.name),
                        sanitize::terminal_text(&s.pane_id),
                        if s.alive { "alive" } else { "dead" },
                        sanitize::terminal_text(&s.cwd),
                        sanitize::terminal_text(&s.command),
                        format_attach_state(s.attached_clients),
                        sanitize::terminal_text(parent_pane_display(&s))
                    );
                }
            }
            Ok(())
        }
        Commands::Ps { target, json } => {
            let processes = client::process_tree(target.as_deref())?;
            if json {
                println!("{}", client::json_pretty(&processes));
            } else {
                println!("SESSION\tPANE\tPID\tPPID\tCPU\tMEM\tRSS_KIB\tETIME\tCOMMAND");
                for process in processes {
                    let indent = "  ".repeat(process.depth);
                    println!(
                        "{}\t{}\t{}\t{}\t{:.1}\t{:.1}\t{}\t{}\t{}{}",
                        sanitize::terminal_text(&process.session),
                        sanitize::terminal_text(&process.pane_id),
                        process.pid,
                        process.ppid,
                        process.cpu_percent,
                        process.mem_percent,
                        process.rss_kib,
                        sanitize::terminal_text(&process.elapsed),
                        indent,
                        sanitize::terminal_text(&process.command)
                    );
                }
            }
            Ok(())
        }
        Commands::Kill { target } => client::kill(&target),
        Commands::Send {
            target,
            text,
            enter,
        } => {
            let mut bytes = text.into_bytes();
            if enter {
                bytes.push(b'\r');
            }
            client::send(&target, bytes)
        }
        Commands::Capture { target, start } => {
            print!("{}", client::capture(&target, start)?);
            Ok(())
        }
        Commands::Shutdown => client::shutdown(),
        Commands::InstallShim => tmux_compat::install_shim(),
        Commands::Env => tmux_compat::print_env_exports(),
        Commands::TmuxCompat { args } => {
            let code = tmux_compat::run_tmux_compat(args)?;
            std::process::exit(code);
        }
        Commands::Notify {
            title,
            subtitle,
            body,
        } => notify(&title, subtitle.as_deref(), &body),
        Commands::Omx { args } => run_agent_command("omx", args),
        Commands::Omc { args } => run_agent_command("omc", args),
        Commands::Ssh {
            host,
            target,
            ssh_args,
        } => ssh_attach(&host, &target, ssh_args),
    }
}

fn expand_attach_short_flag<I>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    let mut argv: Vec<_> = args.into_iter().collect();
    // `-a` is a thin, pre-clap shortcut for `attach`. Keep it exact and in
    // argv[1] only so later attach parsing remains the single source of truth.
    if argv.get(1).is_some_and(|arg| arg == "-a") {
        argv[1] = OsString::from("attach");
    }
    argv
}

fn normalize_command(mut command: Vec<String>) -> Result<Option<String>> {
    if command.first().is_some_and(|s| s == "--") {
        command.remove(0);
    }
    if command.is_empty() {
        Ok(None)
    } else {
        Ok(Some(client::shell_join(&command)?))
    }
}

fn collapse_aliases(mut sessions: Vec<protocol::SessionInfo>) -> Vec<protocol::SessionInfo> {
    sessions.sort_by(|a, b| a.created_unix_ms.cmp(&b.created_unix_ms));
    let mut seen = std::collections::HashSet::new();
    sessions.retain(|s| seen.insert(s.pane_id.clone()));
    sessions
}

#[derive(Debug, Clone, Copy)]
enum ListScope {
    Roots,
    Children,
    All,
}

impl ListScope {
    fn from_flags(all: bool, children: bool) -> Self {
        if all {
            Self::All
        } else if children {
            Self::Children
        } else {
            Self::Roots
        }
    }
}

fn filter_list_sessions(
    sessions: Vec<protocol::SessionInfo>,
    scope: ListScope,
) -> Vec<protocol::SessionInfo> {
    sessions
        .into_iter()
        .filter(|session| match scope {
            ListScope::Roots => session.parent_pane_id.is_none(),
            ListScope::Children => session.parent_pane_id.is_some(),
            ListScope::All => true,
        })
        .collect()
}

fn format_attach_state(attached_clients: usize) -> String {
    match attached_clients {
        0 => "detached".to_string(),
        1 => "attached".to_string(),
        count => format!("attached:{count}"),
    }
}

fn parent_pane_display(session: &protocol::SessionInfo) -> &str {
    session.parent_pane_id.as_deref().unwrap_or("-")
}

fn run_agent_command(binary: &str, args: Vec<String>) -> Result<()> {
    let binary_path =
        client::find_command(binary).with_context(|| format!("{binary} not found in PATH"))?;
    tmux_compat::ensure_shim()?;
    let mut cmd = Vec::with_capacity(args.len() + 1);
    cmd.push(
        binary_path
            .to_str()
            .with_context(|| format!("{binary} resolved to a non-UTF-8 path"))?
            .to_string(),
    );
    cmd.extend(args);
    let command = client::shell_join(&cmd)?;
    let base_name = format!("{binary}-lterm");
    let mut last_conflict = None;
    for _ in 0..32 {
        let session_name = next_agent_session_name(&base_name)?;
        match client::new_session(
            Some(session_name),
            Some(command.clone()),
            None,
            HashMap::new(),
            true,
        ) {
            Ok(info) => return client::attach(&info.pane_id, true, AttachStdinEof::KeepAttached),
            Err(err) if is_session_name_conflict(&err) => last_conflict = Some(err),
            Err(err) => return Err(err),
        }
    }
    Err(last_conflict
        .unwrap_or_else(|| anyhow::anyhow!("could not allocate session name for {base_name}")))
}

fn is_session_name_conflict(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("session name already exists:")
}

fn next_agent_session_name(base_name: &str) -> Result<String> {
    let used: HashSet<_> = client::list_sessions()?
        .into_iter()
        .map(|session| session.name)
        .collect();
    if !used.contains(base_name) {
        return Ok(base_name.to_string());
    }
    for index in 1..=999 {
        let candidate = format!("{base_name}-{index}");
        if !used.contains(&candidate) {
            return Ok(candidate);
        }
    }
    bail!("no available session name for {base_name}");
}

fn notify(title: &str, subtitle: Option<&str>, body: &str) -> Result<()> {
    if client::command_exists("cmux") {
        let mut cmd = Command::new("cmux");
        cmd.arg("notify")
            .arg("--title")
            .arg(title)
            .arg("--body")
            .arg(body);
        if let Some(subtitle) = subtitle {
            cmd.arg("--subtitle").arg(subtitle);
        }
        if cmd.status().is_ok_and(|s| s.success()) {
            return Ok(());
        }
    }

    // cmux and several terminals understand OSC 777. This is intentionally
    // written to stdout so it passes through lterm attach unchanged.
    let fallback_body = subtitle
        .filter(|subtitle| !subtitle.is_empty())
        .map(|subtitle| format!("{subtitle}\n{body}"))
        .unwrap_or_else(|| body.to_string());
    print!(
        "\x1b]777;notify;{};{}\x07",
        sanitize::osc_field(title),
        sanitize::osc_field(&fallback_body)
    );
    std::io::stdout().flush().ok();
    Ok(())
}

fn ssh_attach(host: &str, target: &str, ssh_args: Vec<String>) -> Result<()> {
    validate_ssh_host(host)?;
    let mut command = Command::new("ssh");
    for arg in ssh_args {
        command.arg(arg);
    }
    command.arg("-t").arg("--").arg(host).arg(format!(
        "lterm attach-or-new {}",
        tmux_compat::quote(target)
    ));
    let status = command.status().context("run ssh")?;
    if status.success() {
        Ok(())
    } else {
        bail!("ssh exited with {status}")
    }
}

fn validate_ssh_host(host: &str) -> Result<()> {
    if host.is_empty() {
        bail!("ssh host cannot be empty");
    }
    if host.starts_with('-') {
        bail!("ssh host cannot start with '-'");
    }
    if host.chars().any(|ch| ch.is_control()) {
        bail!("ssh host cannot contain control characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn expand_attach_short_flag_rewrites_only_first_exact_dash_a() {
        assert_eq!(
            expand_attach_short_flag(os_args(&["lterm", "-a", "api"])),
            os_args(&["lterm", "attach", "api"])
        );
        assert_eq!(
            expand_attach_short_flag(os_args(&["lterm", "a", "api"])),
            os_args(&["lterm", "a", "api"])
        );
        assert_eq!(
            expand_attach_short_flag(os_args(&["lterm", "new", "-a", "api"])),
            os_args(&["lterm", "new", "-a", "api"])
        );
        assert_eq!(
            expand_attach_short_flag(os_args(&["lterm", "-a=api"])),
            os_args(&["lterm", "-a=api"])
        );
        assert_eq!(
            expand_attach_short_flag(os_args(&["lterm"])),
            os_args(&["lterm"])
        );
    }
}
