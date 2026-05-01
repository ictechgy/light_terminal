mod client;
mod paths;
mod protocol;
mod sanitize;
mod server;
mod tmux_compat;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
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
    /// Create a detached persistent session.
    New {
        #[arg(short, long)]
        name: Option<String>,
        #[arg(short = 'c', long)]
        cwd: Option<String>,
        #[arg(long)]
        tmux: bool,
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
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Attach to a persistent session or pane.
    Attach {
        #[arg(default_value = "%0")]
        target: String,
    },
    /// Attach to a session, creating it first when missing.
    AttachOrNew {
        #[arg(default_value = "main")]
        target: String,
    },
    /// List sessions.
    List {
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
    let cli = Cli::parse();
    match cli.command {
        Commands::Daemon => server::serve_forever(),
        Commands::New {
            name,
            cwd,
            tmux,
            command,
        } => {
            if tmux {
                tmux_compat::install_shim()?;
            }
            let command = normalize_command(command)?;
            let info = client::new_session(name, command, cwd, HashMap::new(), tmux)?;
            println!("{}\t{}\t{}", info.name, info.pane_id, info.command);
            Ok(())
        }
        Commands::Run {
            name,
            cwd,
            tmux,
            command,
        } => {
            if tmux {
                tmux_compat::install_shim()?;
            }
            let command = normalize_command(command)?.context("run requires a command")?;
            let info = client::new_session(name, Some(command), cwd, HashMap::new(), tmux)?;
            client::attach(&info.pane_id)
        }
        Commands::Attach { target } => client::attach(&target),
        Commands::AttachOrNew { target } => {
            client::attach(&client::attach_or_new(&target)?.pane_id)
        }
        Commands::List { json } => {
            let sessions = client::list_sessions()?;
            if json {
                println!("{}", client::json_pretty(&sessions));
            } else {
                for s in collapse_aliases(sessions) {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        sanitize::terminal_text(&s.name),
                        sanitize::terminal_text(&s.pane_id),
                        if s.alive { "alive" } else { "dead" },
                        sanitize::terminal_text(&s.cwd),
                        sanitize::terminal_text(&s.command)
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
                bytes.push(b'\n');
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
    let mut seen = std::collections::HashSet::new();
    sessions.retain(|s| seen.insert(s.pane_id.clone()));
    sessions.sort_by(|a, b| a.created_unix_ms.cmp(&b.created_unix_ms));
    sessions
}

fn run_agent_command(binary: &str, args: Vec<String>) -> Result<()> {
    if !client::command_exists(binary) {
        bail!("{binary} not found in PATH");
    }
    tmux_compat::install_shim()?;
    let mut cmd = Vec::with_capacity(args.len() + 1);
    cmd.push(binary.to_string());
    cmd.extend(args);
    let command = client::shell_join(&cmd)?;
    let info = client::new_session(
        Some(format!("{binary}-lterm")),
        Some(command),
        None,
        HashMap::new(),
        true,
    )?;
    client::attach(&info.pane_id)
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
    print!(
        "\x1b]777;notify;{};{}\x07",
        sanitize::osc_field(title),
        sanitize::osc_field(body)
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
