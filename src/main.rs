mod client;
mod paths;
mod protocol;
mod sanitize;
mod server;
mod tmux_compat;

#[cfg(test)]
static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
static TEST_ATTACH_FLAG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell as CompletionOutputShell, generate};
use client::{AttachStdinEof, ComposeOptions};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(
    name = "lterm",
    version,
    about = "Light Terminal: a lightweight tmux-compatible session daemon",
    after_help = "Compatibility: lterm -a <target> is equivalent to lterm resume <target>."
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
    #[command(name = "start", visible_alias = "new")]
    New {
        /// Session name to use instead of an auto-generated name.
        #[arg(short, long)]
        name: Option<String>,
        /// Working directory for the session command.
        #[arg(short = 'c', long)]
        cwd: Option<String>,
        /// Expose the lterm tmux compatibility shim inside the session (off by default).
        #[arg(long)]
        tmux: bool,
        /// Create the session without attaching to it.
        #[arg(short = 'd', long)]
        detach: bool,
        /// Disable the lterm status bar while attached.
        #[arg(long)]
        no_status: bool,
        /// Status bar theme stored on this session (alias: --status-color).
        #[arg(long, alias = "status-color", value_name = "THEME", value_parser = parse_status_theme_arg)]
        status_theme: Option<protocol::StatusTheme>,
        /// Shell command to run in the session; defaults to the user's shell when omitted.
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Create a tmux-shimmed session and attach to it.
    Run {
        /// Session name to use instead of an auto-generated name.
        #[arg(short, long)]
        name: Option<String>,
        /// Working directory for the session command.
        #[arg(short = 'c', long)]
        cwd: Option<String>,
        #[arg(long, hide = true)]
        tmux: bool,
        /// Disable the lterm tmux compatibility shim for this run session (enabled by default).
        #[arg(long)]
        no_tmux: bool,
        /// Disable the lterm status bar while attached.
        #[arg(long)]
        no_status: bool,
        /// Status bar theme stored on this session (alias: --status-color).
        #[arg(long, alias = "status-color", value_name = "THEME", value_parser = parse_status_theme_arg)]
        status_theme: Option<protocol::StatusTheme>,
        /// Required shell command to run in the tmux-compatible session.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Resume a persistent session or pane.
    #[command(name = "resume", visible_aliases = ["attach", "a"])]
    Resume {
        /// Session or pane target to resume.
        #[arg(default_value = "%0")]
        target: String,
        /// Disable the lterm status bar while attached.
        #[arg(long)]
        no_status: bool,
    },
    /// Attach to a session, creating it first when missing.
    #[command(name = "open", visible_alias = "attach-or-new")]
    AttachOrNew {
        /// Session or pane target to attach or create.
        #[arg(default_value = "main")]
        target: String,
        /// Disable the lterm status bar while attached.
        #[arg(long)]
        no_status: bool,
    },
    /// List sessions.
    #[command(name = "sessions", visible_aliases = ["list", "ls"])]
    Sessions {
        /// Print sessions as a JSON array for automation.
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
    #[command(visible_alias = "ps")]
    Processes {
        /// Optional session or pane target to inspect.
        target: Option<String>,
        /// Print process rows as a JSON array for automation.
        #[arg(long)]
        json: bool,
        /// Include same-process-group rows that escaped the child tree.
        #[arg(long)]
        orphans: bool,
    },
    /// Diagnose daemon, shim, and version state.
    #[command(visible_alias = "status")]
    Doctor {
        /// Print the diagnostic report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Build a redacted local diagnostic bundle.
    #[command(group(
        ArgGroup::new("diagnose_mode")
            .required(true)
            .args(["bundle"])
    ))]
    Diagnose {
        /// Print a JSON bundle with doctor, session, process, and local environment diagnostics.
        #[arg(long)]
        bundle: bool,
    },
    /// Close a session or pane.
    #[command(name = "close", visible_alias = "kill")]
    Close {
        /// Session or pane target to close.
        target: String,
    },
    /// Rename an existing session without restarting its PTY.
    Rename {
        /// Session or pane target whose session metadata should be renamed.
        target: String,
        /// New session name for future target lookup.
        name: String,
    },
    /// Set or clear a session status bar theme for future attaches.
    #[command(name = "status-theme", visible_alias = "theme")]
    StatusTheme {
        /// Session or pane target to update; pane ids resolve to their session.
        target: String,
        /// Theme name, or `default` to use the attaching client's default.
        theme: String,
    },
    /// Write text to a session or pane.
    #[command(name = "input", visible_alias = "send")]
    Input {
        /// Session or pane target to receive input.
        target: String,
        /// Text to send to the target PTY.
        text: String,
        /// Append Enter after the text.
        #[arg(long)]
        enter: bool,
    },
    /// Capture scrollback from a session or pane.
    #[command(name = "logs", visible_alias = "capture")]
    Logs {
        /// Session or pane target to capture.
        target: String,
        /// Starting scrollback line offset, matching tmux -S semantics.
        #[arg(short = 'S', long, allow_hyphen_values = true)]
        start: Option<i32>,
        /// Inclusive ending scrollback line offset, matching tmux -E semantics.
        #[arg(short = 'E', long, allow_hyphen_values = true)]
        end: Option<i32>,
    },
    /// Record raw PTY output chunks from a session to a local JSONL trace file.
    #[command(visible_alias = "record")]
    Trace {
        /// Session or pane target to trace.
        target: String,
        /// JSONL file to create with raw output chunks encoded as hex.
        #[arg(short, long)]
        output: std::path::PathBuf,
        /// How long to record, e.g. 500ms, 5s, 1m. Required so traces cannot hang forever.
        #[arg(long, value_name = "DURATION", value_parser = parse_wait_duration_arg)]
        duration: Duration,
        /// Maximum raw PTY bytes to record before ending the trace.
        #[arg(long, value_name = "BYTES", default_value_t = client::default_trace_max_bytes(), value_parser = parse_trace_max_bytes_arg)]
        max_bytes: u64,
        /// Overwrite an existing trace file.
        #[arg(long)]
        force: bool,
    },
    /// Compose input while viewing sanitized session output.
    #[command(name = "compose", visible_alias = "mobile")]
    Compose {
        /// Session or pane target to review and receive committed input.
        target: String,
        /// Number of sanitized scrollback lines to show.
        #[arg(long, default_value = "80", value_parser = parse_compose_tail_arg)]
        tail: usize,
        /// Refresh interval for the interactive sanitized output view.
        #[arg(long, value_name = "DURATION", default_value = "500ms", value_parser = parse_wait_duration_arg)]
        refresh: Duration,
        /// Run one capture/send cycle for automation and tests.
        #[arg(long)]
        once: bool,
        /// Text to commit in --once mode.
        #[arg(long, value_name = "TEXT")]
        message: Option<String>,
        /// Do not append Enter (carriage return) when committing input.
        #[arg(long)]
        no_enter: bool,
    },
    /// Wait until a session exits or sanitized output contains text.
    #[command(group(
        ArgGroup::new("wait_condition")
            .required(true)
            .args(["wait_for_exit", "contains"])
    ))]
    Wait {
        /// Session or pane target to observe.
        target: String,
        /// Wait until the session leader exits.
        #[arg(long = "exit", conflicts_with = "contains")]
        wait_for_exit: bool,
        /// Wait until sanitized scrollback contains this text.
        #[arg(long, value_name = "TEXT", conflicts_with = "wait_for_exit")]
        contains: Option<String>,
        /// Maximum wait time, e.g. 250ms, 2s, 5m, 1h. Bare numbers are seconds.
        #[arg(long, value_name = "DURATION", value_parser = parse_wait_duration_arg)]
        timeout: Option<Duration>,
        /// Limit --contains scans to the last N sanitized scrollback lines.
        #[arg(long, value_parser = parse_wait_tail_arg, requires = "contains", conflicts_with = "wait_for_exit")]
        tail: Option<usize>,
        /// Print a machine-readable JSON result.
        #[arg(long)]
        json: bool,
    },
    /// Watch a session and optionally send a notification when the condition is met.
    #[command(group(
        ArgGroup::new("watch_condition")
            .required(true)
            .args(["wait_for_exit", "contains"])
    ))]
    Watch {
        /// Session or pane target to observe.
        target: String,
        /// Watch until the session leader exits.
        #[arg(long = "exit", conflicts_with = "contains")]
        wait_for_exit: bool,
        /// Watch until sanitized scrollback contains this text.
        #[arg(long, value_name = "TEXT", conflicts_with = "wait_for_exit")]
        contains: Option<String>,
        /// Maximum watch time, e.g. 250ms, 2s, 5m, 1h. Bare numbers are seconds.
        #[arg(long, value_name = "DURATION", value_parser = parse_wait_duration_arg)]
        timeout: Option<Duration>,
        /// Limit --contains scans to the last N sanitized scrollback lines.
        #[arg(long, value_parser = parse_wait_tail_arg, requires = "contains", conflicts_with = "wait_for_exit")]
        tail: Option<usize>,
        /// Print a machine-readable JSON result.
        #[arg(long)]
        json: bool,
        /// Send a cmux-friendly notification when the condition is met.
        #[arg(long)]
        notify: bool,
    },
    /// Stop the daemon and all sessions.
    Shutdown,
    /// Install the tmux compatibility shim and print the shim directory.
    InstallShim,
    /// Print shell exports for tmux compatibility.
    Env {
        /// Shell syntax to emit; defaults to POSIX exports for existing eval usage.
        #[arg(long, value_enum)]
        shell: Option<ShellKind>,
    },
    /// Generate shell completion scripts.
    Completions {
        /// Shell completion format to generate.
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// Print a no-touch setup preview for enabling lterm locally.
    Init {
        /// Shell syntax to show in the preview; detected from SHELL when omitted.
        #[arg(long, value_enum)]
        shell: Option<ShellKind>,
    },
    /// tmux-compatible command surface used by the shim.
    TmuxCompat {
        /// Arguments forwarded to the tmux compatibility parser.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Send a cmux-friendly notification.
    Notify {
        /// Notification title.
        #[arg(long)]
        title: String,
        /// Optional notification subtitle.
        #[arg(long)]
        subtitle: Option<String>,
        /// Notification body text.
        #[arg(long, default_value = "")]
        body: String,
    },
    /// Run Oh My Codex inside a tmux-compatible lterm session.
    Omx {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to omx; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Oh My Claude inside a tmux-compatible lterm session.
    Omc {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to omc; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// List agent launcher profiles, default settings, and PATH availability.
    Agents {
        /// Print profiles as a JSON array.
        #[arg(long)]
        json: bool,
        /// JSON file with additional configured custom agent profiles.
        #[arg(long = "agent-config")]
        agent_config: Option<String>,
        /// Optional built-in, configured, or PATH-resolved custom profile names to inspect.
        profiles: Vec<String>,
    },
    /// Run a built-in, configured, or PATH-resolved agent CLI profile inside a tmux-compatible lterm session.
    Agent {
        /// Built-in, configured, or PATH-resolved custom profile name, e.g. claude, codex, opencode, agy.
        profile: String,
        /// JSON file with additional configured custom agent profiles.
        #[arg(long = "agent-config")]
        agent_config: Option<String>,
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to the agent CLI; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Antigravity CLI (agy) inside a tmux-compatible lterm session.
    Agy {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to agy; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Aider inside a tmux-compatible lterm session.
    Aider {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to aider; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Amp CLI inside a tmux-compatible lterm session.
    Amp {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to amp; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Claude Code inside a tmux-compatible lterm session.
    Claude {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to claude; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Codex CLI inside a tmux-compatible lterm session.
    Codex {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to codex; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run GitHub Copilot CLI inside a tmux-compatible lterm session.
    Copilot {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to copilot; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Crush inside a tmux-compatible lterm session.
    Crush {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to crush; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Cursor Agent CLI inside a tmux-compatible lterm session.
    CursorAgent {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to cursor-agent; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Gemini CLI inside a tmux-compatible lterm session.
    Gemini {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to gemini; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Goose CLI inside a tmux-compatible lterm session.
    Goose {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to goose; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Jules Tools CLI inside a tmux-compatible lterm session.
    Jules {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to jules; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Kiro CLI inside a tmux-compatible lterm session.
    Kiro {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to kiro-cli; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Kimi CLI inside a tmux-compatible lterm session.
    Kimi {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to kimi; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run OpenCode inside a tmux-compatible lterm session.
    Opencode {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to opencode; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Qwen CLI inside a tmux-compatible lterm session.
    Qwen {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to qwen; use `--` before args that look like lterm options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Attach to lterm on a remote host over SSH. Requires lterm installed remotely.
    Ssh {
        /// SSH host to connect to.
        host: String,
        /// Remote session or pane target to attach.
        #[arg(default_value = "main")]
        target: String,
        /// Additional ssh arguments after `--`.
        #[arg(last = true)]
        ssh_args: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ShellKind {
    Bash,
    Zsh,
    Fish,
    Posix,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
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
            status_theme,
            command,
        } => {
            if tmux {
                tmux_compat::ensure_shim()?;
            }
            let command = normalize_command(command)?;
            let info = client::new_session(name, command, cwd, HashMap::new(), status_theme, tmux)?;
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
            // Hidden no-op kept for callers that already pass `run --tmux`.
            // `--no-tmux` intentionally wins so old aliases can opt out later.
            tmux: _,
            no_tmux,
            no_status,
            status_theme,
            command,
        } => {
            let tmux = !no_tmux;
            if tmux {
                tmux_compat::ensure_shim()?;
            }
            let command = normalize_command(command)?.context("run requires a command")?;
            let info =
                client::new_session(name, Some(command), cwd, HashMap::new(), status_theme, tmux)?;
            client::attach(&info.pane_id, !no_status, AttachStdinEof::KeepAttached)
        }
        Commands::Resume { target, no_status } => {
            client::attach(&target, !no_status, AttachStdinEof::Detach)
        }
        Commands::AttachOrNew { target, no_status } => client::attach(
            &client::attach_or_new(&target)?.pane_id,
            !no_status,
            AttachStdinEof::Detach,
        ),
        Commands::Sessions {
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
        Commands::Processes {
            target,
            json,
            orphans,
        } => {
            let processes = client::process_tree(target.as_deref(), orphans)?;
            if json {
                println!("{}", client::json_pretty(&processes));
            } else {
                println!(
                    "SESSION\tPANE\tPID\tPPID\tPGID\tORPHAN\tCPU\tMEM\tRSS_KIB\tETIME\tCOMMAND"
                );
                for process in processes {
                    let indent = "  ".repeat(process.depth);
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{:.1}\t{:.1}\t{}\t{}\t{}{}",
                        sanitize::terminal_text(&process.session),
                        sanitize::terminal_text(&process.pane_id),
                        process.pid,
                        process.ppid,
                        process
                            .process_group_id
                            .map_or_else(|| "-".to_string(), |pgid| pgid.to_string()),
                        if process.orphan { "yes" } else { "no" },
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
        Commands::Doctor { json } => print_doctor_report(json),
        Commands::Diagnose { bundle: _ } => print_diagnose_bundle(),
        Commands::Close { target } => client::kill(&target),
        Commands::Rename { target, name } => {
            let info = client::rename_session(&target, &name)?;
            println!(
                "{}\t{}",
                sanitize::terminal_text(&info.name),
                sanitize::terminal_text(&info.pane_id)
            );
            Ok(())
        }
        Commands::StatusTheme { target, theme } => {
            let status_theme = parse_status_theme_setting(&theme)?;
            let info = client::set_status_theme(&target, status_theme)?;
            println!(
                "{}\t{}\t{}",
                sanitize::terminal_text(&info.name),
                sanitize::terminal_text(&info.pane_id),
                format_status_theme(info.status_theme)
            );
            Ok(())
        }
        Commands::Input {
            target,
            text,
            enter,
        } => client::send(&target, input_commit_bytes(text, enter)),
        Commands::Logs { target, start, end } => {
            let output = if end.is_some() {
                client::capture_range(&target, start, end)?
            } else {
                client::capture(&target, start)?
            };
            print!("{output}");
            Ok(())
        }
        Commands::Trace {
            target,
            output,
            duration,
            max_bytes,
            force,
        } => client::trace_output(&target, &output, duration, max_bytes, force),
        Commands::Compose {
            target,
            tail,
            refresh,
            once,
            message,
            no_enter,
        } => client::compose(
            &target,
            ComposeOptions {
                tail,
                refresh,
                once,
                message,
                append_enter: !no_enter,
            },
        ),
        Commands::Wait {
            target,
            wait_for_exit,
            contains,
            timeout,
            tail,
            json,
        } => run_wait_command(
            &target,
            wait_for_exit,
            contains.as_deref(),
            timeout,
            tail,
            json,
            false,
        ),
        Commands::Watch {
            target,
            wait_for_exit,
            contains,
            timeout,
            tail,
            json,
            notify,
        } => run_wait_command(
            &target,
            wait_for_exit,
            contains.as_deref(),
            timeout,
            tail,
            json,
            notify,
        ),
        Commands::Shutdown => client::shutdown(),
        Commands::InstallShim => tmux_compat::install_shim(),
        Commands::Env { shell } => {
            tmux_compat::print_env_exports(shell.unwrap_or(ShellKind::Posix).into())
        }
        Commands::Completions { shell } => print_completions(shell),
        Commands::Init { shell } => print_init_preview(shell.unwrap_or_else(detect_init_shell)),
        Commands::TmuxCompat { args } => {
            let code = tmux_compat::run_tmux_compat(args)?;
            std::process::exit(code);
        }
        Commands::Notify {
            title,
            subtitle,
            body,
        } => notify(&title, subtitle.as_deref(), &body),
        Commands::Omx { launch, args } => {
            run_agent_profile(AgentProfile::known("omx"), launch, args)
        }
        Commands::Omc { launch, args } => {
            run_agent_profile(AgentProfile::known("omc"), launch, args)
        }
        Commands::Agents {
            json,
            agent_config,
            profiles,
        } => print_agent_profiles(json, agent_config.as_deref(), profiles),
        Commands::Agent {
            profile,
            agent_config,
            launch,
            args,
        } => run_agent_profile(
            resolve_agent_profile(&profile, agent_config.as_deref())?,
            launch,
            args,
        ),
        Commands::Agy { launch, args } => {
            run_agent_profile(AgentProfile::known("agy"), launch, args)
        }
        Commands::Aider { launch, args } => {
            run_agent_profile(AgentProfile::known("aider"), launch, args)
        }
        Commands::Amp { launch, args } => {
            run_agent_profile(AgentProfile::known("amp"), launch, args)
        }
        Commands::Claude { launch, args } => {
            run_agent_profile(AgentProfile::known("claude"), launch, args)
        }
        Commands::Codex { launch, args } => {
            run_agent_profile(AgentProfile::known("codex"), launch, args)
        }
        Commands::Copilot { launch, args } => {
            run_agent_profile(AgentProfile::known("copilot"), launch, args)
        }
        Commands::Crush { launch, args } => {
            run_agent_profile(AgentProfile::known("crush"), launch, args)
        }
        Commands::CursorAgent { launch, args } => {
            run_agent_profile(AgentProfile::known("cursor-agent"), launch, args)
        }
        Commands::Gemini { launch, args } => {
            run_agent_profile(AgentProfile::known("gemini"), launch, args)
        }
        Commands::Goose { launch, args } => {
            run_agent_profile(AgentProfile::known("goose"), launch, args)
        }
        Commands::Jules { launch, args } => {
            run_agent_profile(AgentProfile::known("jules"), launch, args)
        }
        Commands::Kiro { launch, args } => {
            run_agent_profile(AgentProfile::known("kiro"), launch, args)
        }
        Commands::Kimi { launch, args } => {
            run_agent_profile(AgentProfile::known("kimi"), launch, args)
        }
        Commands::Opencode { launch, args } => {
            run_agent_profile(AgentProfile::known("opencode"), launch, args)
        }
        Commands::Qwen { launch, args } => {
            run_agent_profile(AgentProfile::known("qwen"), launch, args)
        }
        Commands::Ssh {
            host,
            target,
            ssh_args,
        } => ssh_attach(&host, &target, ssh_args),
    }
}

impl From<ShellKind> for tmux_compat::EnvShell {
    fn from(value: ShellKind) -> Self {
        match value {
            ShellKind::Bash | ShellKind::Zsh | ShellKind::Posix => Self::Posix,
            ShellKind::Fish => Self::Fish,
        }
    }
}

impl From<CompletionShell> for CompletionOutputShell {
    fn from(value: CompletionShell) -> Self {
        match value {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Zsh => Self::Zsh,
            CompletionShell::Fish => Self::Fish,
        }
    }
}

fn print_completions(shell: CompletionShell) -> Result<()> {
    let mut command = Cli::command();
    let binary_name = command.get_name().to_string();
    let generator: CompletionOutputShell = shell.into();
    generate(generator, &mut command, binary_name, &mut std::io::stdout());
    Ok(())
}

fn detect_init_shell() -> ShellKind {
    let Some(shell) = std::env::var_os("SHELL") else {
        return ShellKind::Posix;
    };
    let name = Path::new(&shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    match name {
        "bash" => ShellKind::Bash,
        "zsh" => ShellKind::Zsh,
        "fish" => ShellKind::Fish,
        _ => ShellKind::Posix,
    }
}

fn print_init_preview(shell: ShellKind) -> Result<()> {
    let (shell_name, enable_command) = match shell {
        ShellKind::Bash => ("bash", "eval \"$(lterm env)\""),
        ShellKind::Zsh => ("zsh", "eval \"$(lterm env)\""),
        ShellKind::Fish => ("fish", "lterm env --shell fish | source"),
        ShellKind::Posix => ("posix", "eval \"$(lterm env)\""),
    };
    println!("lterm init preview");
    println!("shell\t{shell_name}");
    println!("modifies_files\tno");
    println!("step\t1\tlterm doctor --json");
    println!("step\t2\tlterm install-shim");
    println!("step\t3\t{enable_command}");
    println!("note\tCopy the enable command into a trusted shell startup file only after review.");
    println!("note\tRun lterm doctor --json again after changing PATH to verify shim_dir_in_path.");
    Ok(())
}

const WAIT_TIMEOUT_EXIT_CODE: i32 = 124;

#[derive(Debug, Serialize)]
struct WaitOutcome {
    target: String,
    event: &'static str,
    matched: bool,
    timed_out: bool,
    elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    exited: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    needle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<protocol::SessionInfo>,
}

fn run_wait_command(
    target: &str,
    wait_for_exit: bool,
    contains: Option<&str>,
    timeout: Option<Duration>,
    tail: Option<usize>,
    json: bool,
    notify_on_match: bool,
) -> Result<()> {
    let started = Instant::now();
    let outcome = if wait_for_exit {
        wait_for_exit_condition(target, timeout, started)?
    } else if let Some(needle) = contains {
        wait_for_contains_condition(target, needle, timeout, tail, started)?
    } else {
        bail!("wait/watch requires either --exit or --contains");
    };

    print_wait_outcome(&outcome, json);
    if notify_on_match && outcome.matched && !outcome.timed_out {
        notify_wait_outcome(&outcome, json)?;
    }
    if outcome.timed_out {
        std::process::exit(WAIT_TIMEOUT_EXIT_CODE);
    }
    if !outcome.matched {
        std::process::exit(1);
    }
    Ok(())
}

fn wait_for_exit_condition(
    target: &str,
    timeout: Option<Duration>,
    started: Instant,
) -> Result<WaitOutcome> {
    let result = client::wait_exit(target, timeout)?;
    Ok(WaitOutcome {
        target: target.to_string(),
        event: "exit",
        matched: result.exited,
        timed_out: result.timed_out,
        elapsed_ms: elapsed_millis(started),
        exited: Some(result.exited),
        exit_code: result.session.exit_code,
        needle: None,
        session: Some(result.session),
    })
}

fn wait_for_contains_condition(
    target: &str,
    needle: &str,
    timeout: Option<Duration>,
    tail: Option<usize>,
    started: Instant,
) -> Result<WaitOutcome> {
    let needle = validate_wait_needle(needle)?;
    let start_line = wait_tail_start(tail)?;
    let result = client::wait_contains(target, needle, start_line, timeout)?;
    Ok(WaitOutcome {
        target: target.to_string(),
        event: "contains",
        matched: result.matched,
        timed_out: result.timed_out,
        elapsed_ms: elapsed_millis(started),
        exited: Some(result.exited),
        exit_code: result.session.exit_code,
        needle: Some(needle.to_string()),
        session: None,
    })
}

fn validate_wait_needle(needle: &str) -> Result<&str> {
    if needle.is_empty() {
        bail!("--contains text cannot be empty");
    }
    Ok(needle)
}

fn wait_tail_start(tail: Option<usize>) -> Result<Option<i32>> {
    let Some(tail) = tail else {
        return Ok(None);
    };
    let tail = i32::try_from(tail).context("--tail exceeds supported scrollback range")?;
    Ok(Some(-tail))
}

fn print_wait_outcome(outcome: &WaitOutcome, json: bool) {
    if json {
        println!("{}", client::json_pretty(outcome));
        return;
    }

    println!("STATUS\tEVENT\tTARGET\tELAPSED_MS\tDETAIL");
    let status = if outcome.timed_out {
        "timeout"
    } else if outcome.matched {
        "matched"
    } else if outcome.exited == Some(true) {
        "exited"
    } else {
        "pending"
    };
    let detail = if outcome.event == "exit" {
        outcome
            .exit_code
            .map(|code| format!("exit_code={code}"))
            .unwrap_or_else(|| "exit_code=unknown".to_string())
    } else {
        outcome
            .needle
            .as_deref()
            .map(|needle| format!("contains={}", sanitize::terminal_text(needle)))
            .unwrap_or_else(|| "contains=<none>".to_string())
    };
    println!(
        "{}\t{}\t{}\t{}\t{}",
        status,
        outcome.event,
        sanitize::terminal_text(&outcome.target),
        outcome.elapsed_ms,
        detail
    );
}

fn notify_wait_outcome(outcome: &WaitOutcome, preserve_stdout: bool) -> Result<()> {
    let body = if outcome.event == "exit" {
        let status = outcome
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        format!("session {} exited with status {status}", outcome.target)
    } else {
        let needle = outcome.needle.as_deref().unwrap_or_default();
        format!("session {} output matched {needle}", outcome.target)
    };
    notify_with_fallback(
        "lterm watch matched",
        Some(outcome.event),
        &sanitize::osc_field(&body),
        if preserve_stdout {
            NotificationFallback::Stderr
        } else {
            NotificationFallback::Stdout
        },
    )
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    client_version: &'static str,
    client_protocol_version: u32,
    daemon_reachable: bool,
    daemon_version: Option<String>,
    daemon_protocol_version: Option<u32>,
    version_match: Option<bool>,
    daemon_session_count: Option<u64>,
    daemon_active_connections: Option<u64>,
    daemon_shutting_down: Option<bool>,
    // 같은 OS 사용자 trust boundary 식별자. 옛 데몬은 보고하지 않으므로 Option.
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_uid: Option<u32>,
    // 데몬 시작 시각(UNIX epoch seconds). 옛 데몬은 없음.
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_started_at_unix_secs: Option<u64>,
    // 데몬 가동 시간(초). started_at과 현재 시각 차이로 계산.
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_uptime_secs: Option<u64>,
    daemon_error: Option<String>,
    // 비정상 상태(unreachable, version mismatch, shutting down 등)에 대한
    // 사람·에이전트 가독 한 줄 요약. 정상이면 None.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    runtime_dir: String,
    data_dir: String,
    socket_path: String,
    shim_dir: String,
    tmux_shim_path: String,
    tmux_shim_exists: bool,
    shim_dir_in_path: bool,
}

#[derive(Debug, Serialize)]
struct DiagnosticPrivacy {
    raw_pty_streams_included: bool,
    sanitized_scrollback_included: bool,
    environment_values_redacted: bool,
    session_commands_redacted: bool,
    process_commands_redacted: bool,
    paths_summarized: bool,
    notes: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct DiagnosticPathSummary {
    home_relative: bool,
    component_count: usize,
    basename: Option<String>,
}

#[derive(Debug, Serialize)]
struct DiagnosticDoctor {
    client_version: &'static str,
    client_protocol_version: u32,
    daemon_reachable: bool,
    daemon_version: Option<String>,
    daemon_protocol_version: Option<u32>,
    version_match: Option<bool>,
    daemon_session_count: Option<u64>,
    daemon_active_connections: Option<u64>,
    daemon_shutting_down: Option<bool>,
    daemon_uid: Option<u32>,
    daemon_started_at_unix_secs: Option<u64>,
    daemon_uptime_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    runtime_dir: DiagnosticPathSummary,
    data_dir: DiagnosticPathSummary,
    socket_path: DiagnosticPathSummary,
    shim_dir: DiagnosticPathSummary,
    tmux_shim_path: DiagnosticPathSummary,
    tmux_shim_exists: bool,
    shim_dir_in_path: bool,
}

#[derive(Debug, Serialize)]
struct DiagnosticEnvironment {
    cwd: Option<DiagnosticPathSummary>,
    shell: Option<String>,
    term: Option<String>,
    ssh: bool,
    tmux_set: bool,
    cmux_context_set: bool,
    lterm_socket_set: bool,
    lterm_pane_set: bool,
    lterm_parent_token_set: bool,
    path_contains_shim_dir: bool,
}

#[derive(Debug, Serialize)]
struct DiagnosticSession {
    id: String,
    name: String,
    pane_id: String,
    command: String,
    cwd: DiagnosticPathSummary,
    created_unix_ms: u128,
    alive: bool,
    exit_code: Option<i32>,
    rows: u16,
    cols: u16,
    parent_pane_id: Option<String>,
    parent_session_id: Option<String>,
    attached_clients: usize,
    process_id: Option<u32>,
    process_group_id: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_theme: Option<protocol::StatusTheme>,
}

#[derive(Debug, Serialize)]
struct DiagnosticProcess {
    session: String,
    pane_id: String,
    depth: usize,
    pid: u32,
    ppid: u32,
    process_group_id: Option<i32>,
    orphan: bool,
    stat: String,
    cpu_percent: f32,
    mem_percent: f32,
    rss_kib: u64,
    elapsed: String,
    command: String,
}

#[derive(Debug, Serialize)]
struct DiagnosticBundle {
    schema_version: &'static str,
    generated_at_unix_secs: Option<u64>,
    privacy: DiagnosticPrivacy,
    doctor: DiagnosticDoctor,
    environment: DiagnosticEnvironment,
    sessions: Option<Vec<DiagnosticSession>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sessions_error: Option<String>,
    processes: Option<Vec<DiagnosticProcess>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    processes_error: Option<String>,
    notes: Vec<String>,
}

fn build_doctor_report() -> Result<DoctorReport> {
    let runtime_dir = paths::runtime_dir()?;
    let data_dir = paths::data_dir()?;
    let socket_path = paths::socket_path()?;
    let shim_dir = paths::shim_dir()?;
    let tmux_shim_path = shim_dir.join("tmux");
    let (daemon, daemon_reachable, daemon_error) = match client::daemon_status() {
        Ok(status) => (Some(status), true, None),
        Err(err) => {
            let reachable = client::daemon_ping().is_ok();
            (None, reachable, Some(err.to_string()))
        }
    };
    let version_match = daemon.as_ref().map(|status| {
        status.version == env!("CARGO_PKG_VERSION")
            && status.protocol_version == protocol::PROTOCOL_VERSION
    });
    let daemon_uid = daemon.as_ref().and_then(|status| status.daemon_uid);
    // 옛 buggy 빌드가 sentinel 0을 보낸 경우(이전 PR 회귀)를 invalid로 걸러낸다
    // — quad-review 합의 fix. 새 빌드는 clock failure 시 None을 전송하므로 정상
    // 케이스에서는 filter가 no-op.
    let daemon_started_at_unix_secs = daemon
        .as_ref()
        .and_then(|status| status.started_at_unix_secs)
        .filter(|&secs| secs > 0);
    // uptime은 started_at이 valid일 때만 계산. 옛 데몬(필드 없음) 또는 clock failure
    // 케이스에서는 None을 그대로 두어 doctor JSON이 50+년 같은 misleading 값을
    // 보고하지 않게 한다.
    let daemon_uptime_secs = daemon_started_at_unix_secs.and_then(|started| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs().saturating_sub(started))
    });
    let daemon_shutting_down = daemon.as_ref().map(|status| status.shutting_down);
    let tmux_shim_exists = tmux_shim_path.is_file();
    // 비정상 상태 단일 요약. 우선순위: unreachable → shutdown → version mismatch
    // → status RPC 실패(reachable이지만 daemon_error) → shim missing.
    // daemon_error 분기는 quad-review 합의 fix: socket은 reachable이지만 Status가
    // 실패한 경우에도 reason을 채워 doctor 계약("abnormal에는 reason")을 지킨다.
    let reason = if !daemon_reachable {
        Some(
            "Daemon is not reachable. It usually auto-starts on the next `lterm` command; run `lterm doctor` again or inspect daemon startup with `lterm logs`."
                .to_string(),
        )
    } else if matches!(daemon_shutting_down, Some(true)) {
        Some(
            "Daemon is shutting down. Wait for it to exit, then rerun any pending command."
                .to_string(),
        )
    } else if matches!(version_match, Some(false)) {
        Some(
            "Client version or protocol does not match the running daemon. Run `lterm shutdown` and retry to load the new daemon binary."
                .to_string(),
        )
    } else if let Some(err) = daemon_error.as_deref() {
        Some(format!(
            "Daemon socket is reachable but a status RPC failed: {}. Run `lterm doctor` again or check `lterm logs` for daemon errors.",
            sanitize::terminal_text(err)
        ))
    } else if !tmux_shim_exists {
        Some(
            "Tmux shim is missing — tmux-compatible agents may fall back to system tmux. Run `lterm install-shim` (or reinstall lterm) to recreate it."
                .to_string(),
        )
    } else {
        None
    };
    Ok(DoctorReport {
        client_version: env!("CARGO_PKG_VERSION"),
        client_protocol_version: protocol::PROTOCOL_VERSION,
        daemon_reachable,
        daemon_version: daemon.as_ref().map(|status| status.version.clone()),
        daemon_protocol_version: daemon.as_ref().map(|status| status.protocol_version),
        version_match,
        daemon_session_count: daemon.as_ref().map(|status| status.session_count),
        daemon_active_connections: daemon.as_ref().map(|status| status.active_connections),
        daemon_shutting_down,
        daemon_uid,
        daemon_started_at_unix_secs,
        daemon_uptime_secs,
        daemon_error,
        reason,
        runtime_dir: runtime_dir.display().to_string(),
        data_dir: data_dir.display().to_string(),
        socket_path: socket_path.display().to_string(),
        shim_dir: shim_dir.display().to_string(),
        tmux_shim_path: tmux_shim_path.display().to_string(),
        tmux_shim_exists,
        shim_dir_in_path: path_contains_dir(&shim_dir),
    })
}

fn print_doctor_report(json: bool) -> Result<()> {
    let report = build_doctor_report()?;

    if json {
        println!("{}", client::json_pretty(&report));
    } else {
        println!("client_version\t{}", report.client_version);
        println!(
            "client_protocol_version\t{}",
            report.client_protocol_version
        );
        println!("daemon_reachable\t{}", yes_no(report.daemon_reachable));
        println!(
            "daemon_version\t{}",
            report
                .daemon_version
                .as_deref()
                .map(sanitize::terminal_text)
                .unwrap_or_else(|| "-".to_string())
        );
        println!(
            "daemon_protocol_version\t{}",
            report
                .daemon_protocol_version
                .map_or_else(|| "-".to_string(), |version| version.to_string())
        );
        println!(
            "version_match\t{}",
            report
                .version_match
                .map_or("-", |matches| if matches { "yes" } else { "no" })
        );
        println!(
            "daemon_session_count\t{}",
            report
                .daemon_session_count
                .map_or_else(|| "-".to_string(), |count| count.to_string())
        );
        println!(
            "daemon_active_connections\t{}",
            report
                .daemon_active_connections
                .map_or_else(|| "-".to_string(), |count| count.to_string())
        );
        println!(
            "daemon_shutting_down\t{}",
            report
                .daemon_shutting_down
                .map_or("-", |value| if value { "yes" } else { "no" })
        );
        println!(
            "daemon_uid\t{}",
            report
                .daemon_uid
                .map_or_else(|| "-".to_string(), |uid| uid.to_string())
        );
        println!(
            "daemon_uptime_secs\t{}",
            report
                .daemon_uptime_secs
                .map_or_else(|| "-".to_string(), |secs| secs.to_string())
        );
        if let Some(error) = &report.daemon_error {
            println!("daemon_error\t{}", sanitize::terminal_text(error));
        }
        if let Some(reason) = &report.reason {
            println!("reason\t{}", sanitize::terminal_text(reason));
        }
        println!(
            "runtime_dir\t{}",
            sanitize::terminal_text(&report.runtime_dir)
        );
        println!("data_dir\t{}", sanitize::terminal_text(&report.data_dir));
        println!(
            "socket_path\t{}",
            sanitize::terminal_text(&report.socket_path)
        );
        println!("shim_dir\t{}", sanitize::terminal_text(&report.shim_dir));
        println!(
            "tmux_shim_path\t{}",
            sanitize::terminal_text(&report.tmux_shim_path)
        );
        println!("tmux_shim_exists\t{}", yes_no(report.tmux_shim_exists));
        println!("shim_dir_in_path\t{}", yes_no(report.shim_dir_in_path));
    }
    Ok(())
}

fn print_diagnose_bundle() -> Result<()> {
    let doctor = build_doctor_report()?;
    let mut notes = vec![
        "diagnose --bundle is local-only and does not include raw PTY bytes or scrollback by default".to_string(),
    ];

    let (sessions, sessions_error) = if doctor.daemon_reachable {
        match client::rpc::<Vec<protocol::SessionInfo>>(&protocol::Request::List) {
            Ok(sessions) => (
                Some(
                    collapse_aliases(sessions)
                        .into_iter()
                        .map(redact_session_info)
                        .collect(),
                ),
                None,
            ),
            Err(err) => (None, Some(sanitize::terminal_text(&err.to_string()))),
        }
    } else {
        notes.push("daemon is not reachable; skipped session and process collection without auto-starting it".to_string());
        (None, None)
    };

    let (processes, processes_error) = if sessions.is_some() {
        match client::process_tree(None, true) {
            Ok(processes) => (
                Some(processes.into_iter().map(redact_process_info).collect()),
                None,
            ),
            Err(err) => (None, Some(sanitize::terminal_text(&err.to_string()))),
        }
    } else {
        (None, None)
    };

    let environment = diagnostic_environment(&doctor);
    let bundle = DiagnosticBundle {
        schema_version: "1.0",
        generated_at_unix_secs: current_unix_secs(),
        privacy: DiagnosticPrivacy {
            raw_pty_streams_included: false,
            sanitized_scrollback_included: false,
            environment_values_redacted: true,
            session_commands_redacted: true,
            process_commands_redacted: true,
            paths_summarized: true,
            notes: vec![
                "environment section reports presence flags for sensitive lterm/tmux/cmux variables",
                "filesystem paths are summarized instead of emitted as full absolute paths",
                "session/process command fields keep only the executable basename plus an argument-redaction marker",
                "no raw terminal scrollback or PTY bytes are included",
            ],
        },
        doctor: redact_doctor_report(&doctor),
        environment,
        sessions,
        sessions_error,
        processes,
        processes_error,
        notes,
    };
    println!("{}", client::json_pretty(&bundle));
    Ok(())
}

fn current_unix_secs() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

fn diagnostic_environment(doctor: &DoctorReport) -> DiagnosticEnvironment {
    DiagnosticEnvironment {
        cwd: std::env::current_dir()
            .ok()
            .map(|cwd| diagnostic_path_summary(cwd.as_path())),
        shell: std::env::var_os("SHELL").and_then(|shell| {
            Path::new(&shell)
                .file_name()
                .map(|name| sanitize::terminal_text(&name.to_string_lossy()))
        }),
        term: sanitized_env_value("TERM"),
        ssh: std::env::var_os("SSH_CONNECTION").is_some()
            || std::env::var_os("SSH_CLIENT").is_some()
            || std::env::var_os("SSH_TTY").is_some(),
        tmux_set: std::env::var_os("TMUX").is_some(),
        cmux_context_set: std::env::var_os("CMUX_SURFACE_ID").is_some()
            || std::env::var_os("CMUX_WORKSPACE_ID").is_some()
            || std::env::var_os("CMUX_WINDOW_ID").is_some(),
        lterm_socket_set: std::env::var_os("LTERM_SOCKET").is_some(),
        lterm_pane_set: std::env::var_os("LTERM_PANE").is_some(),
        lterm_parent_token_set: std::env::var_os("LTERM_PARENT_TOKEN").is_some(),
        path_contains_shim_dir: path_contains_dir(Path::new(&doctor.shim_dir)),
    }
}

fn redact_doctor_report(report: &DoctorReport) -> DiagnosticDoctor {
    DiagnosticDoctor {
        client_version: report.client_version,
        client_protocol_version: report.client_protocol_version,
        daemon_reachable: report.daemon_reachable,
        daemon_version: report.daemon_version.clone(),
        daemon_protocol_version: report.daemon_protocol_version,
        version_match: report.version_match,
        daemon_session_count: report.daemon_session_count,
        daemon_active_connections: report.daemon_active_connections,
        daemon_shutting_down: report.daemon_shutting_down,
        daemon_uid: report.daemon_uid,
        daemon_started_at_unix_secs: report.daemon_started_at_unix_secs,
        daemon_uptime_secs: report.daemon_uptime_secs,
        daemon_error: report.daemon_error.as_deref().map(sanitize::terminal_text),
        reason: report.reason.as_deref().map(sanitize::terminal_text),
        runtime_dir: diagnostic_path_summary(Path::new(&report.runtime_dir)),
        data_dir: diagnostic_path_summary(Path::new(&report.data_dir)),
        socket_path: diagnostic_path_summary(Path::new(&report.socket_path)),
        shim_dir: diagnostic_path_summary(Path::new(&report.shim_dir)),
        tmux_shim_path: diagnostic_path_summary(Path::new(&report.tmux_shim_path)),
        tmux_shim_exists: report.tmux_shim_exists,
        shim_dir_in_path: report.shim_dir_in_path,
    }
}

fn redact_session_info(session: protocol::SessionInfo) -> DiagnosticSession {
    DiagnosticSession {
        id: session.id,
        name: sanitize::terminal_text(&session.name),
        pane_id: sanitize::terminal_text(&session.pane_id),
        command: redact_command_summary(&session.command),
        cwd: diagnostic_path_summary(Path::new(&session.cwd)),
        created_unix_ms: session.created_unix_ms,
        alive: session.alive,
        exit_code: session.exit_code,
        rows: session.rows,
        cols: session.cols,
        parent_pane_id: session
            .parent_pane_id
            .map(|value| sanitize::terminal_text(&value)),
        parent_session_id: session
            .parent_session_id
            .map(|value| sanitize::terminal_text(&value)),
        attached_clients: session.attached_clients,
        process_id: session.process_id,
        process_group_id: session.process_group_id,
        status_theme: session.status_theme,
    }
}

fn redact_process_info(process: client::ProcessInfo) -> DiagnosticProcess {
    DiagnosticProcess {
        session: sanitize::terminal_text(&process.session),
        pane_id: sanitize::terminal_text(&process.pane_id),
        depth: process.depth,
        pid: process.pid,
        ppid: process.ppid,
        process_group_id: process.process_group_id,
        orphan: process.orphan,
        stat: sanitize::terminal_text(&process.stat),
        cpu_percent: process.cpu_percent,
        mem_percent: process.mem_percent,
        rss_kib: process.rss_kib,
        elapsed: sanitize::terminal_text(&process.elapsed),
        command: redact_command_summary(&process.command),
    }
}

fn diagnostic_path_summary(path: &Path) -> DiagnosticPathSummary {
    let home_relative = std::env::var_os("HOME")
        .map(|home| path.starts_with(Path::new(&home)))
        .unwrap_or(false);
    DiagnosticPathSummary {
        home_relative,
        component_count: path.components().count(),
        basename: path
            .file_name()
            .map(|name| sanitize::terminal_text(&name.to_string_lossy())),
    }
}

fn redact_command_summary(command: &str) -> String {
    let mut saw_redacted_prefix = false;
    let mut has_more = false;
    let mut program = None;
    let mut parts = command.split_whitespace();
    for token in parts.by_ref() {
        let trimmed = token.trim_matches(['\'', '"']);
        if is_shell_assignment_token(trimmed) {
            saw_redacted_prefix = true;
            continue;
        }
        has_more = parts.next().is_some() || saw_redacted_prefix;
        program = safe_command_basename(trimmed);
        break;
    }
    let program = program.unwrap_or_else(|| "<redacted>".to_string());
    if has_more || saw_redacted_prefix {
        format!("{program} …")
    } else {
        program
    }
}

fn is_shell_assignment_token(token: &str) -> bool {
    let Some((name, _value)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn safe_command_basename(token: &str) -> Option<String> {
    let basename = Path::new(token).file_name()?.to_string_lossy();
    let sanitized = sanitize::terminal_text(&basename);
    let mut chars = sanitized.chars();
    let first = chars.next()?;
    let safe_first = first.is_ascii_alphanumeric() || matches!(first, '_' | '.' | '-');
    let safe_rest =
        chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-' | '+'));
    (safe_first && safe_rest).then_some(sanitized)
}

fn sanitized_env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| sanitize::terminal_text(&value))
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn path_contains_dir(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|entry| entry == dir))
        .unwrap_or(false)
}

fn expand_attach_short_flag<I>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    let mut argv: Vec<_> = args.into_iter().collect();
    // `-a` is a thin, pre-clap shortcut for `resume`. Keep it exact and in
    // argv[1] only so later resume parsing remains the single source of truth.
    if argv.get(1).is_some_and(|arg| arg == "-a") {
        argv[1] = OsString::from("resume");
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

fn parse_status_theme_arg(value: &str) -> std::result::Result<protocol::StatusTheme, String> {
    protocol::StatusTheme::parse(value).ok_or_else(|| {
        format!(
            "invalid status theme {value:?}; expected one of: {}",
            protocol::StatusTheme::allowed_values()
        )
    })
}

fn parse_wait_duration_arg(value: &str) -> std::result::Result<Duration, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("duration cannot be empty".to_string());
    }
    if value.starts_with('-') {
        return Err("duration must be positive".to_string());
    }

    let lower = value.to_ascii_lowercase();
    let (number, unit_millis) = if let Some(number) = lower.strip_suffix("ms") {
        (number, 1_u64)
    } else if let Some(number) = lower.strip_suffix('s') {
        (number, 1_000_u64)
    } else if let Some(number) = lower.strip_suffix('m') {
        (number, 60_000_u64)
    } else if let Some(number) = lower.strip_suffix('h') {
        (number, 3_600_000_u64)
    } else {
        (lower.as_str(), 1_000_u64)
    };

    let number = number.parse::<u64>().map_err(|_| {
        format!(
            "invalid duration {value:?}; expected a positive integer with optional ms/s/m/h suffix"
        )
    })?;
    if number == 0 {
        return Err("duration must be greater than zero".to_string());
    }
    let millis = number
        .checked_mul(unit_millis)
        .ok_or_else(|| format!("duration {value:?} is too large"))?;
    Ok(Duration::from_millis(millis))
}

fn parse_trace_max_bytes_arg(value: &str) -> std::result::Result<u64, String> {
    let value = value.trim();
    let bytes = value
        .parse::<u64>()
        .map_err(|_| format!("invalid max bytes {value:?}; expected a positive integer"))?;
    if bytes == 0 {
        return Err("--max-bytes must be greater than zero".to_string());
    }
    Ok(bytes)
}

fn parse_wait_tail_arg(value: &str) -> std::result::Result<usize, String> {
    let value = value.trim();
    let tail = value
        .parse::<usize>()
        .map_err(|_| format!("invalid tail {value:?}; expected a positive integer"))?;
    if tail == 0 {
        return Err("--tail must be greater than zero".to_string());
    }
    Ok(tail)
}

fn parse_compose_tail_arg(value: &str) -> std::result::Result<usize, String> {
    let tail = parse_wait_tail_arg(value)?;
    i32::try_from(tail)
        .map(|_| tail)
        .map_err(|_| "--tail exceeds supported scrollback range".to_string())
}

fn input_commit_bytes(text: String, enter: bool) -> Vec<u8> {
    let mut bytes = text.into_bytes();
    if enter {
        bytes.push(b'\r');
    }
    bytes
}

fn parse_status_theme_setting(value: &str) -> Result<Option<protocol::StatusTheme>> {
    let trimmed = value.trim();
    if matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "default" | "clear" | "none"
    ) {
        return Ok(None);
    }
    parse_status_theme_arg(trimmed)
        .map(Some)
        .map_err(anyhow::Error::msg)
}

fn format_status_theme(theme: Option<protocol::StatusTheme>) -> &'static str {
    // Safe to print directly: StatusTheme is a fixed allowlist, never user-provided text.
    theme.map_or("default", protocol::StatusTheme::as_str)
}

fn collapse_aliases(mut sessions: Vec<protocol::SessionInfo>) -> Vec<protocol::SessionInfo> {
    sessions.sort_by_key(|session| session.created_unix_ms);
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentProfile {
    name: String,
    binary: String,
    session_base: String,
    show_status: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct AgentProfileInfo {
    profile: String,
    kind: String,
    binary: String,
    session_base: String,
    status_default: bool,
    available: bool,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentProfilesConfig {
    #[serde(default)]
    profiles: Vec<ConfiguredAgentProfile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredAgentProfile {
    name: String,
    #[serde(default)]
    binary: Option<String>,
    #[serde(default)]
    session_base: Option<String>,
    #[serde(default = "default_agent_status")]
    status_default: bool,
}

fn default_agent_status() -> bool {
    true
}

#[derive(Debug, Clone, Default, Args)]
struct AgentLaunchOptions {
    /// Session name to use instead of the profile default.
    #[arg(long)]
    name: Option<String>,
    /// Working directory for the agent process.
    #[arg(long)]
    cwd: Option<String>,
    /// Create the agent session without attaching to it.
    #[arg(long, conflicts_with_all = ["status", "no_status"])]
    detach: bool,
    /// Force-enable the lterm status bar while attached.
    #[arg(long, conflicts_with = "no_status")]
    status: bool,
    /// Disable the lterm status bar while attached.
    #[arg(long, conflicts_with = "status")]
    no_status: bool,
    /// Status bar theme stored on this agent session (alias: --status-color).
    #[arg(long, alias = "status-color", value_name = "THEME", value_parser = parse_status_theme_arg)]
    status_theme: Option<protocol::StatusTheme>,
}

impl AgentLaunchOptions {
    fn session_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    fn detach(&self) -> bool {
        self.detach
    }

    fn show_status(&self, default: bool) -> bool {
        if self.status {
            true
        } else if self.no_status {
            false
        } else {
            default
        }
    }

    fn status_theme(&self) -> Option<protocol::StatusTheme> {
        self.status_theme
    }
}

const BUILT_IN_AGENT_PROFILES: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "copilot",
    "cursor-agent",
    "agy",
    "jules",
    "kiro",
    "aider",
    "goose",
    "amp",
    "crush",
    "gemini",
    "kimi",
    "qwen",
    "omx",
    "omc",
];

impl AgentProfile {
    fn known(name: &str) -> Self {
        match name {
            "claude" => Self {
                name: name.to_string(),
                binary: "claude".to_string(),
                session_base: "claude-lterm".to_string(),
                show_status: false,
            },
            "codex" => Self {
                name: name.to_string(),
                binary: "codex".to_string(),
                session_base: "codex-lterm".to_string(),
                show_status: false,
            },
            "opencode" => Self {
                name: name.to_string(),
                binary: "opencode".to_string(),
                session_base: "opencode-lterm".to_string(),
                show_status: false,
            },
            "copilot" => Self {
                name: name.to_string(),
                binary: "copilot".to_string(),
                session_base: "copilot-lterm".to_string(),
                show_status: false,
            },
            "cursor-agent" => Self {
                name: name.to_string(),
                binary: "cursor-agent".to_string(),
                session_base: "cursor-agent-lterm".to_string(),
                show_status: false,
            },
            "agy" => Self {
                name: name.to_string(),
                binary: "agy".to_string(),
                session_base: "agy-lterm".to_string(),
                show_status: false,
            },
            "jules" => Self {
                name: name.to_string(),
                binary: "jules".to_string(),
                session_base: "jules-lterm".to_string(),
                show_status: false,
            },
            "kiro" => Self {
                name: name.to_string(),
                binary: "kiro-cli".to_string(),
                session_base: "kiro-lterm".to_string(),
                show_status: false,
            },
            "aider" => Self {
                name: name.to_string(),
                binary: "aider".to_string(),
                session_base: "aider-lterm".to_string(),
                show_status: false,
            },
            "goose" => Self {
                name: name.to_string(),
                binary: "goose".to_string(),
                session_base: "goose-lterm".to_string(),
                show_status: false,
            },
            "amp" => Self {
                name: name.to_string(),
                binary: "amp".to_string(),
                session_base: "amp-lterm".to_string(),
                show_status: false,
            },
            "crush" => Self {
                name: name.to_string(),
                binary: "crush".to_string(),
                session_base: "crush-lterm".to_string(),
                show_status: false,
            },
            "gemini" => Self {
                name: name.to_string(),
                binary: "gemini".to_string(),
                session_base: "gemini-lterm".to_string(),
                show_status: false,
            },
            "kimi" => Self {
                name: name.to_string(),
                binary: "kimi".to_string(),
                session_base: "kimi-lterm".to_string(),
                show_status: false,
            },
            "qwen" => Self {
                name: name.to_string(),
                binary: "qwen".to_string(),
                session_base: "qwen-lterm".to_string(),
                show_status: false,
            },
            "omx" => Self {
                name: name.to_string(),
                binary: "omx".to_string(),
                session_base: "omx-lterm".to_string(),
                show_status: true,
            },
            "omc" => Self {
                name: name.to_string(),
                binary: "omc".to_string(),
                session_base: "omc-lterm".to_string(),
                show_status: true,
            },
            _ => unreachable!("unknown built-in agent profile: {name}"),
        }
    }

    fn resolve(profile: &str) -> Result<Self> {
        match profile {
            "claude" => Ok(Self::known("claude")),
            "codex" => Ok(Self::known("codex")),
            "opencode" => Ok(Self::known("opencode")),
            "copilot" => Ok(Self::known("copilot")),
            "cursor-agent" => Ok(Self::known("cursor-agent")),
            "agy" => Ok(Self::known("agy")),
            "jules" => Ok(Self::known("jules")),
            "kiro" => Ok(Self::known("kiro")),
            "aider" => Ok(Self::known("aider")),
            "goose" => Ok(Self::known("goose")),
            "amp" => Ok(Self::known("amp")),
            "crush" => Ok(Self::known("crush")),
            "gemini" => Ok(Self::known("gemini")),
            "kimi" => Ok(Self::known("kimi")),
            "qwen" => Ok(Self::known("qwen")),
            "omx" => Ok(Self::known("omx")),
            "omc" => Ok(Self::known("omc")),
            custom => {
                validate_agent_profile_name(custom)?;
                Ok(Self {
                    name: custom.to_string(),
                    binary: custom.to_string(),
                    session_base: format!("{custom}-lterm"),
                    show_status: true,
                })
            }
        }
    }
}

fn built_in_agent_profile_infos() -> Vec<AgentProfileInfo> {
    BUILT_IN_AGENT_PROFILES
        .iter()
        .map(|name| agent_profile_info(AgentProfile::known(name), "built-in"))
        .collect()
}

fn selected_agent_profile_infos(
    config_path: Option<&str>,
    profiles: Vec<String>,
) -> Result<Vec<AgentProfileInfo>> {
    let config_supplied = config_path.is_some();
    let configured_profiles = load_agent_profiles_config(config_path)?;
    if profiles.is_empty() {
        let mut infos = built_in_agent_profile_infos();
        infos.extend(
            configured_profiles
                .into_iter()
                .map(|profile| agent_profile_info(profile, "configured")),
        );
        return Ok(infos);
    }

    profiles
        .into_iter()
        .map(|profile| {
            if is_built_in_agent_profile(&profile) {
                return Ok(agent_profile_info(
                    AgentProfile::known(&profile),
                    "built-in",
                ));
            }
            if let Some(configured) = configured_profiles
                .iter()
                .find(|configured| configured.name == profile)
            {
                return Ok(agent_profile_info(configured.clone(), "configured"));
            }
            if config_supplied {
                bail!("{}", missing_configured_agent_profile_message(&profile));
            }
            AgentProfile::resolve(&profile).map(|profile| agent_profile_info(profile, "custom"))
        })
        .collect()
}

fn is_built_in_agent_profile(profile: &str) -> bool {
    BUILT_IN_AGENT_PROFILES.contains(&profile)
}

fn agent_profile_info(profile: AgentProfile, kind: &str) -> AgentProfileInfo {
    let path =
        client::find_command(&profile.binary).map(|path| path.to_string_lossy().into_owned());
    AgentProfileInfo {
        profile: profile.name,
        kind: kind.to_string(),
        binary: profile.binary,
        session_base: profile.session_base,
        status_default: profile.show_status,
        available: path.is_some(),
        path,
    }
}

fn print_agent_profiles(
    json: bool,
    config_path: Option<&str>,
    profiles: Vec<String>,
) -> Result<()> {
    let profiles = selected_agent_profile_infos(config_path, profiles)?;
    if json {
        println!("{}", client::json_pretty(&profiles));
        return Ok(());
    }

    println!("PROFILE\tBINARY\tSESSION_BASE\tSTATUS\tAVAILABLE\tPATH\tKIND");
    for profile in profiles {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            sanitize::terminal_text(&profile.profile),
            sanitize::terminal_text(&profile.binary),
            sanitize::terminal_text(&profile.session_base),
            if profile.status_default { "on" } else { "off" },
            if profile.available {
                "available"
            } else {
                "missing"
            },
            sanitize::terminal_text(profile.path.as_deref().unwrap_or("-")),
            sanitize::terminal_text(&profile.kind)
        );
    }
    Ok(())
}

fn resolve_agent_profile(profile: &str, config_path: Option<&str>) -> Result<AgentProfile> {
    let config_supplied = config_path.is_some();
    let configured_profiles = load_agent_profiles_config(config_path)?;
    if is_built_in_agent_profile(profile) {
        return Ok(AgentProfile::known(profile));
    }
    if let Some(configured) = configured_profiles
        .iter()
        .find(|configured| configured.name == profile)
    {
        return Ok(configured.clone());
    }
    if config_supplied {
        bail!("{}", missing_configured_agent_profile_message(profile));
    }
    AgentProfile::resolve(profile)
}

fn missing_configured_agent_profile_message(profile: &str) -> String {
    format!(
        "agent profile {profile:?} was not found in --agent-config; omit --agent-config to use a PATH-resolved custom profile"
    )
}

fn load_agent_profiles_config(config_path: Option<&str>) -> Result<Vec<AgentProfile>> {
    let Some(config_path) = config_path else {
        return Ok(Vec::new());
    };
    let path = Path::new(config_path);
    let source = agent_config_source_label(path);
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("read agent config {source}"))?;
    parse_agent_profiles_config(&contents, &source)
}

fn agent_config_source_label(path: &Path) -> String {
    path.display().to_string().escape_debug().to_string()
}

fn parse_agent_profiles_config(contents: &str, source: &str) -> Result<Vec<AgentProfile>> {
    let config: AgentProfilesConfig =
        serde_json::from_str(contents).with_context(|| format!("parse agent config {source}"))?;
    let mut seen = HashSet::new();
    config
        .profiles
        .into_iter()
        .map(|profile| configured_agent_profile(profile, &mut seen))
        .collect()
}

fn configured_agent_profile(
    profile: ConfiguredAgentProfile,
    seen: &mut HashSet<String>,
) -> Result<AgentProfile> {
    validate_agent_profile_name(&profile.name)
        .with_context(|| format!("invalid configured agent profile {:?}", profile.name))?;
    if is_built_in_agent_profile(&profile.name) {
        bail!(
            "configured agent profile cannot redefine built-in profile {:?}",
            profile.name
        );
    }
    if !seen.insert(profile.name.clone()) {
        bail!("duplicate configured agent profile {:?}", profile.name);
    }

    let binary = profile.binary.unwrap_or_else(|| profile.name.clone());
    validate_agent_profile_name(&binary).with_context(|| {
        format!(
            "invalid binary for configured agent profile {:?}",
            profile.name
        )
    })?;
    let session_base = profile
        .session_base
        .unwrap_or_else(|| format!("{}-lterm", profile.name));
    validate_agent_session_name(&session_base).with_context(|| {
        format!(
            "invalid session_base for configured agent profile {:?}",
            profile.name
        )
    })?;

    Ok(AgentProfile {
        name: profile.name,
        binary,
        session_base,
        show_status: profile.status_default,
    })
}

fn run_agent_profile(
    profile: AgentProfile,
    launch: AgentLaunchOptions,
    mut args: Vec<String>,
) -> Result<()> {
    if let Some(name) = launch.session_name() {
        validate_agent_session_name(name)?;
    }
    if let Some(cwd) = launch.cwd() {
        validate_agent_cwd(cwd)?;
    }
    if args.first().is_some_and(|arg| arg == "--") {
        args.remove(0);
    }
    let binary_path = client::find_command(&profile.binary)
        .with_context(|| format!("{} not found in PATH", profile.binary))?;
    tmux_compat::ensure_shim()?;
    let mut cmd = Vec::with_capacity(args.len() + 1);
    cmd.push(
        binary_path
            .to_str()
            .with_context(|| format!("{} resolved to a non-UTF-8 path", profile.binary))?
            .to_string(),
    );
    cmd.extend(args);
    let command = client::shell_join(&cmd)?;
    let mut last_conflict = None;
    let explicit_session_name = launch.session_name().map(str::to_string);
    let max_attempts = if explicit_session_name.is_some() {
        1
    } else {
        32
    };
    for _ in 0..max_attempts {
        let session_name = match &explicit_session_name {
            Some(name) => name.clone(),
            None => next_agent_session_name(&profile.session_base)?,
        };
        let env = HashMap::from([("LTERM_AGENT".to_string(), profile.name.to_string())]);
        let created = client::new_session(
            Some(session_name),
            Some(command.clone()),
            launch.cwd().map(str::to_string),
            env,
            launch.status_theme(),
            true,
        );
        match created {
            Ok(info) => {
                if launch.detach() {
                    // Machine-readable detach mode reserves stdout for this single TSV record.
                    let mut stdout = std::io::stdout().lock();
                    stdout.write_all(detached_output_line(&info).as_bytes())?;
                    stdout.flush()?;
                    return Ok(());
                }
                return client::attach(
                    &info.pane_id,
                    launch.show_status(profile.show_status),
                    AttachStdinEof::KeepAttached,
                );
            }
            Err(err) if is_session_name_conflict(&err) => {
                last_conflict = Some(if let Some(name) = explicit_session_name.as_deref() {
                    err.context(format!("failed to create agent session named {name}"))
                } else {
                    err
                });
            }
            Err(err) => {
                return if let Some(name) = explicit_session_name.as_deref() {
                    Err(err).with_context(|| format!("failed to create agent session named {name}"))
                } else {
                    Err(err)
                };
            }
        }
    }
    let err = last_conflict.unwrap_or_else(|| {
        if let Some(name) = explicit_session_name.as_deref() {
            anyhow::anyhow!("could not allocate session name {name}")
        } else {
            anyhow::anyhow!(
                "could not allocate session name for {}",
                profile.session_base
            )
        }
    });
    Err(err)
}

fn detached_output_line(info: &protocol::SessionInfo) -> String {
    format!(
        "{}\t{}\t{}\n",
        detached_field(&info.name),
        detached_field(&info.pane_id),
        detached_field(&info.command)
    )
}

fn detached_field(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

fn validate_agent_session_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("agent session name cannot be empty");
    }
    if name.len() > 128 {
        bail!("agent session name cannot exceed 128 bytes");
    }
    if name.starts_with('-') {
        bail!("agent session name cannot start with '-': {name}");
    }
    if name.starts_with('%') {
        bail!("agent session name cannot look like a pane id: {name}");
    }
    if Uuid::parse_str(name).is_ok() {
        bail!("agent session name cannot look like a UUID: {name}");
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        bail!("agent session name may only contain ASCII letters, numbers, '.', '_' and '-'");
    }
    Ok(())
}

fn validate_agent_cwd(cwd: &str) -> Result<()> {
    if cwd.trim().is_empty() {
        bail!("agent cwd cannot be empty");
    }
    if cwd != cwd.trim() {
        bail!("agent cwd cannot have leading or trailing whitespace");
    }
    if cwd.chars().any(|ch| ch == '\0' || ch.is_control()) {
        bail!("agent cwd cannot contain control characters");
    }
    Ok(())
}

fn validate_agent_profile_name(profile: &str) -> Result<()> {
    if profile.is_empty() {
        bail!("agent profile cannot be empty");
    }
    if profile.len() > 64 {
        bail!("agent profile cannot exceed 64 bytes");
    }
    if !profile
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        bail!("agent profile may only contain ASCII letters, numbers, '.', '_' and '-'");
    }
    if profile.starts_with('-') {
        bail!("agent profile cannot start with '-': {profile}");
    }
    Ok(())
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

#[derive(Debug, Clone, Copy)]
enum NotificationFallback {
    Stdout,
    Stderr,
}

fn notify(title: &str, subtitle: Option<&str>, body: &str) -> Result<()> {
    notify_with_fallback(title, subtitle, body, NotificationFallback::Stdout)
}

fn notify_with_fallback(
    title: &str,
    subtitle: Option<&str>,
    body: &str,
    fallback: NotificationFallback,
) -> Result<()> {
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
        if matches!(fallback, NotificationFallback::Stderr) {
            cmd.stdout(std::process::Stdio::null());
        }
        if cmd.status().is_ok_and(|s| s.success()) {
            return Ok(());
        }
    }

    // cmux and several terminals understand OSC 777. The standalone `notify`
    // command writes fallback OSC to stdout so it passes through lterm attach
    // unchanged; JSON-producing watch commands use stderr to keep stdout
    // machine-readable.
    let fallback_body = subtitle
        .filter(|subtitle| !subtitle.is_empty())
        .map(|subtitle| format!("{subtitle}\n{body}"))
        .unwrap_or_else(|| body.to_string());
    let message = format!(
        "\x1b]777;notify;{};{}\x07",
        sanitize::osc_field(title),
        sanitize::osc_field(&fallback_body)
    );
    match fallback {
        NotificationFallback::Stdout => {
            std::io::stdout().write_all(message.as_bytes()).ok();
            std::io::stdout().flush().ok();
        }
        NotificationFallback::Stderr => {
            std::io::stderr().write_all(message.as_bytes()).ok();
            std::io::stderr().flush().ok();
        }
    }
    Ok(())
}

fn ssh_attach(host: &str, target: &str, ssh_args: Vec<String>) -> Result<()> {
    validate_ssh_host(host)?;
    let mut command = Command::new("ssh");
    for arg in ssh_args {
        command.arg(arg);
    }
    command
        .arg("-t")
        .arg("--")
        .arg(host)
        .arg(ssh_remote_command(target));
    let status = command.status().context("run ssh")?;
    if status.success() {
        Ok(())
    } else {
        bail!("ssh exited with {status}")
    }
}

fn ssh_remote_command(target: &str) -> String {
    // Keep the wire command on the older attach-or-new spelling so a newer
    // local lterm can still reach remote hosts that do not know `lterm open`.
    format!("lterm attach-or-new {}", tmux_compat::quote(target))
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
            os_args(&["lterm", "resume", "api"])
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

    #[test]
    fn wait_duration_parser_accepts_documented_units() {
        assert_eq!(
            parse_wait_duration_arg("250ms").expect("ms duration"),
            Duration::from_millis(250)
        );
        assert_eq!(
            parse_wait_duration_arg("2").expect("bare seconds duration"),
            Duration::from_secs(2)
        );
        assert_eq!(
            parse_wait_duration_arg("3s").expect("seconds duration"),
            Duration::from_secs(3)
        );
        assert_eq!(
            parse_wait_duration_arg("4m").expect("minutes duration"),
            Duration::from_secs(240)
        );
        assert_eq!(
            parse_wait_duration_arg("1h").expect("hours duration"),
            Duration::from_secs(3_600)
        );
    }

    #[test]
    fn wait_duration_parser_rejects_invalid_values() {
        for value in ["", "0", "-1s", "1x", "1.5s"] {
            assert!(
                parse_wait_duration_arg(value).is_err(),
                "{value:?} should be rejected"
            );
        }
    }

    #[test]
    fn diagnostic_command_summary_redacts_env_assignment_values() {
        assert_eq!(
            redact_command_summary("AWS_SECRET_ACCESS_KEY=supersecret cargo run"),
            "cargo …"
        );
        assert_eq!(redact_command_summary("TOKEN=supersecret"), "<redacted> …");
        assert_eq!(redact_command_summary("sh -lc 'echo secret'"), "sh …");
        assert_eq!(redact_command_summary("/usr/bin/cargo"), "cargo");
        assert_eq!(redact_command_summary("$(echo secret) arg"), "<redacted> …");
    }

    #[test]
    fn wait_requires_exactly_one_condition() {
        assert!(Cli::try_parse_from(["lterm", "wait", "main"]).is_err());
        assert!(
            Cli::try_parse_from(["lterm", "wait", "main", "--exit", "--contains", "READY"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["lterm", "wait", "main", "--exit"]).is_ok());
        assert!(Cli::try_parse_from(["lterm", "wait", "main", "--contains", "READY"]).is_ok());
    }

    #[test]
    fn watch_accepts_wait_conditions_and_notify() {
        let cli = Cli::try_parse_from([
            "lterm",
            "watch",
            "main",
            "--contains",
            "READY",
            "--timeout",
            "250ms",
            "--tail",
            "10",
            "--json",
            "--notify",
        ])
        .expect("watch command should parse");
        let Commands::Watch {
            target,
            wait_for_exit,
            contains,
            timeout,
            tail,
            json,
            notify,
        } = cli.command
        else {
            panic!("expected watch command");
        };
        assert_eq!(target, "main");
        assert!(!wait_for_exit);
        assert_eq!(contains.as_deref(), Some("READY"));
        assert_eq!(timeout, Some(Duration::from_millis(250)));
        assert_eq!(tail, Some(10));
        assert!(json);
        assert!(notify);
    }

    #[test]
    fn wait_tail_applies_only_to_contains() {
        assert!(
            Cli::try_parse_from([
                "lterm",
                "wait",
                "main",
                "--contains",
                "READY",
                "--tail",
                "1"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["lterm", "wait", "main", "--exit", "--tail", "1"]).is_err());
        assert!(Cli::try_parse_from(["lterm", "watch", "main", "--exit", "--tail", "1"]).is_err());
        assert!(
            Cli::try_parse_from([
                "lterm",
                "wait",
                "main",
                "--contains",
                "READY",
                "--tail",
                "0"
            ])
            .is_err()
        );
    }

    #[test]
    fn compose_parses_mobile_alias_and_validates_tail() {
        let cli = Cli::try_parse_from([
            "lterm",
            "mobile",
            "main",
            "--tail",
            "10",
            "--refresh",
            "250ms",
            "--once",
            "--message",
            "hello",
            "--no-enter",
        ])
        .expect("mobile alias should parse");
        let Commands::Compose {
            target,
            tail,
            refresh,
            once,
            message,
            no_enter,
        } = cli.command
        else {
            panic!("expected compose command");
        };
        assert_eq!(target, "main");
        assert_eq!(tail, 10);
        assert_eq!(refresh, Duration::from_millis(250));
        assert!(once);
        assert_eq!(message.as_deref(), Some("hello"));
        assert!(no_enter);

        assert!(Cli::try_parse_from(["lterm", "compose", "main", "--tail", "0"]).is_err());
        assert!(Cli::try_parse_from(["lterm", "compose", "main", "--tail", "2147483648"]).is_err());
        assert!(Cli::try_parse_from(["lterm", "compose", "main", "--refresh", "0ms"]).is_err());
    }

    #[test]
    fn input_enter_writes_carriage_return() {
        assert_eq!(input_commit_bytes("hello".to_string(), true), b"hello\r");
        assert_eq!(input_commit_bytes("hello".to_string(), false), b"hello");
    }

    #[test]
    fn ssh_remote_command_uses_compatibility_name_and_quotes_target() {
        assert_eq!(ssh_remote_command("main"), "lterm attach-or-new main");
        assert_eq!(
            ssh_remote_command("agent main"),
            "lterm attach-or-new 'agent main'"
        );
        assert!(
            !ssh_remote_command("main").contains("lterm open"),
            "ssh wire command must remain compatible with older remotes"
        );
    }

    #[test]
    fn known_agent_profiles_define_terminal_policy() {
        for (name, binary, session_base, show_status) in [
            ("claude", "claude", "claude-lterm", false),
            ("codex", "codex", "codex-lterm", false),
            ("opencode", "opencode", "opencode-lterm", false),
            ("copilot", "copilot", "copilot-lterm", false),
            ("cursor-agent", "cursor-agent", "cursor-agent-lterm", false),
            ("agy", "agy", "agy-lterm", false),
            ("jules", "jules", "jules-lterm", false),
            ("kiro", "kiro-cli", "kiro-lterm", false),
            ("aider", "aider", "aider-lterm", false),
            ("goose", "goose", "goose-lterm", false),
            ("amp", "amp", "amp-lterm", false),
            ("crush", "crush", "crush-lterm", false),
            ("gemini", "gemini", "gemini-lterm", false),
            ("kimi", "kimi", "kimi-lterm", false),
            ("qwen", "qwen", "qwen-lterm", false),
            ("omx", "omx", "omx-lterm", true),
            ("omc", "omc", "omc-lterm", true),
        ] {
            let profile = AgentProfile::resolve(name).expect("built-in profile");
            assert_eq!(profile.binary, binary, "{name}");
            assert_eq!(profile.session_base, session_base, "{name}");
            assert_eq!(profile.show_status, show_status, "{name}");
        }
    }

    #[test]
    fn built_in_agent_profile_infos_match_launcher_contract() {
        let infos = built_in_agent_profile_infos();
        let names: Vec<_> = infos.iter().map(|info| info.profile.as_str()).collect();
        assert_eq!(
            names,
            [
                "claude",
                "codex",
                "opencode",
                "copilot",
                "cursor-agent",
                "agy",
                "jules",
                "kiro",
                "aider",
                "goose",
                "amp",
                "crush",
                "gemini",
                "kimi",
                "qwen",
                "omx",
                "omc",
            ]
        );

        let claude = infos
            .iter()
            .find(|info| info.profile == "claude")
            .expect("claude profile info");
        assert_eq!(claude.kind, "built-in");
        assert_eq!(claude.binary, "claude");
        assert_eq!(claude.session_base, "claude-lterm");
        assert!(!claude.status_default);

        let omc = infos
            .iter()
            .find(|info| info.profile == "omc")
            .expect("omc profile info");
        assert_eq!(omc.binary, "omc");
        assert_eq!(omc.session_base, "omc-lterm");
        assert!(omc.status_default);
    }

    #[test]
    fn selected_agent_profile_infos_include_custom_profiles() {
        let infos =
            selected_agent_profile_infos(None, vec!["codex".to_string(), "my-agent".to_string()])
                .expect("selected agent profile infos");
        assert_eq!(infos.len(), 2);

        let codex = &infos[0];
        assert_eq!(codex.profile, "codex");
        assert_eq!(codex.kind, "built-in");
        assert_eq!(codex.session_base, "codex-lterm");
        assert!(!codex.status_default);

        let custom = &infos[1];
        assert_eq!(custom.profile, "my-agent");
        assert_eq!(custom.kind, "custom");
        assert_eq!(custom.binary, "my-agent");
        assert_eq!(custom.session_base, "my-agent-lterm");
        assert!(custom.status_default);

        assert!(selected_agent_profile_infos(None, vec!["../agent".to_string()]).is_err());
    }

    #[test]
    fn agent_config_profiles_are_validated_and_resolved() {
        let profiles = parse_agent_profiles_config(
            r#"{
                "profiles": [
                    {
                        "name": "repo-review",
                        "binary": "codex",
                        "session_base": "repo-review-session",
                        "status_default": false
                    },
                    { "name": "helper" }
                ]
            }"#,
            "inline test config",
        )
        .expect("agent config");
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].name, "repo-review");
        assert_eq!(profiles[0].binary, "codex");
        assert_eq!(profiles[0].session_base, "repo-review-session");
        assert!(!profiles[0].show_status);
        assert_eq!(profiles[1].name, "helper");
        assert_eq!(profiles[1].binary, "helper");
        assert_eq!(profiles[1].session_base, "helper-lterm");
        assert!(profiles[1].show_status);

        let infos =
            selected_agent_profile_infos(None, vec!["codex".to_string(), "my-agent".to_string()])
                .expect("selected infos without config");
        assert_eq!(infos[1].kind, "custom");

        assert!(
            parse_agent_profiles_config(
                r#"{ "profiles": [{ "name": "codex", "binary": "codex" }] }"#,
                "inline test config",
            )
            .is_err()
        );
        assert!(
            parse_agent_profiles_config(
                r#"{ "profiles": [{ "name": "../bad", "binary": "codex" }] }"#,
                "inline test config",
            )
            .is_err()
        );
        assert!(
            parse_agent_profiles_config(
                r#"{ "profiles": [{ "name": "dup" }, { "name": "dup" }] }"#,
                "inline test config",
            )
            .is_err()
        );
        assert!(
            parse_agent_profiles_config(
                r#"{ "profiles": [{ "name": "null-status", "status_default": null }] }"#,
                "inline test config",
            )
            .is_err()
        );
        assert_eq!(
            missing_configured_agent_profile_message("typo-agent"),
            r#"agent profile "typo-agent" was not found in --agent-config; omit --agent-config to use a PATH-resolved custom profile"#
        );
        assert_eq!(
            agent_config_source_label(Path::new("bad-\u{1b}[31m-agents.json")),
            "bad-\\u{1b}[31m-agents.json"
        );
    }

    #[test]
    fn custom_agent_profile_uses_profile_as_binary() {
        let profile = AgentProfile::resolve("my-agent").expect("custom profile");
        assert_eq!(profile.name, "my-agent");
        assert_eq!(profile.binary, "my-agent");
        assert_eq!(profile.session_base, "my-agent-lterm");
        assert!(profile.show_status);
    }

    #[test]
    fn agent_launch_status_flags_override_profile_default() {
        let default = AgentLaunchOptions::default();
        assert!(default.show_status(true));
        assert!(!default.show_status(false));

        let status = AgentLaunchOptions {
            status: true,
            ..AgentLaunchOptions::default()
        };
        assert!(status.show_status(false));

        let no_status = AgentLaunchOptions {
            no_status: true,
            ..AgentLaunchOptions::default()
        };
        assert!(!no_status.show_status(true));
    }

    #[test]
    fn agent_launch_options_keep_agent_short_flags_as_args() {
        let cli = Cli::try_parse_from(["lterm", "codex", "-c", "agent-config"])
            .expect("agent short flags should pass through without --");
        match cli.command {
            Commands::Codex { launch, args } => {
                assert_eq!(launch.cwd(), None);
                assert_eq!(args, vec!["-c", "agent-config"]);
            }
            other => panic!("expected codex command, got {other:?}"),
        }
    }

    #[test]
    fn agent_launch_options_parse_long_controls_before_separator() {
        let cli = Cli::try_parse_from([
            "lterm",
            "agent",
            "codex",
            "--name",
            "repo-agent",
            "--cwd",
            "/tmp",
            "--detach",
            "--",
            "-c",
            "agent-config",
        ])
        .expect("long launch controls should parse before --");
        match cli.command {
            Commands::Agent {
                profile,
                agent_config,
                launch,
                args,
            } => {
                assert_eq!(profile, "codex");
                assert_eq!(agent_config, None);
                assert_eq!(launch.session_name(), Some("repo-agent"));
                assert_eq!(launch.cwd(), Some("/tmp"));
                assert!(launch.detach());
                assert_eq!(args, vec!["-c", "agent-config"]);
            }
            other => panic!("expected agent command, got {other:?}"),
        }
    }

    #[test]
    fn agent_launch_detach_conflicts_with_status_flags() {
        assert!(
            Cli::try_parse_from(["lterm", "codex", "--detach", "--status"]).is_err(),
            "--status applies to attach and should not be accepted with --detach"
        );
        assert!(
            Cli::try_parse_from(["lterm", "codex", "--detach", "--no-status"]).is_err(),
            "--no-status applies to attach and should not be accepted with --detach"
        );
    }

    #[test]
    fn detached_output_fields_are_tab_and_line_safe() {
        assert_eq!(detached_field("safe"), "safe");
        assert_eq!(detached_field("tab\tline\nesc\u{1b}"), "tab line esc ");
        assert_eq!(
            detached_field("line\u{2028}paragraph\u{2029}"),
            "line paragraph "
        );

        let line = detached_output_line(&protocol::SessionInfo {
            id: "id".into(),
            name: "name\twith\ncontrols".into(),
            pane_id: "%7".into(),
            command: "cmd\targ\nnext".into(),
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
        });
        assert_eq!(line.matches('\t').count(), 2, "{line:?}");
        assert!(line.ends_with('\n'), "{line:?}");
        assert_eq!(line, "name with controls\t%7\tcmd arg next\n");
    }

    #[test]
    fn explicit_agent_session_names_use_session_safe_syntax() {
        assert!(validate_agent_session_name("repo.agent_1").is_ok());
        assert!(validate_agent_session_name("").is_err());
        assert!(validate_agent_session_name("-bad").is_err());
        assert!(validate_agent_session_name("%0").is_err());
        assert!(validate_agent_session_name("bad/name").is_err());
        assert!(validate_agent_session_name("bad name").is_err());
        assert!(validate_agent_session_name("550e8400-e29b-41d4-a716-446655440000").is_err());
        assert!(validate_agent_session_name(&"a".repeat(129)).is_err());
    }

    #[test]
    fn agent_cwd_rejects_empty_or_control_paths() {
        assert!(validate_agent_cwd("/tmp/repo").is_ok());
        assert!(validate_agent_cwd("relative repo").is_ok());
        assert!(validate_agent_cwd("").is_err());
        assert!(validate_agent_cwd("   ").is_err());
        assert!(validate_agent_cwd(" /tmp/repo").is_err());
        assert!(validate_agent_cwd("/tmp/repo ").is_err());
        assert!(validate_agent_cwd("/tmp/repo\nnext").is_err());
    }

    #[test]
    fn custom_agent_profile_rejects_shell_or_path_syntax() {
        assert!(AgentProfile::resolve("../claude").is_err());
        assert!(AgentProfile::resolve("claude code").is_err());
        assert!(AgentProfile::resolve("-claude").is_err());
        let err = AgentProfile::resolve("-\u{1b}[31m")
            .expect_err("control bytes in rejected profile must not be echoed")
            .to_string();
        assert!(!err.contains('\u{1b}'), "{err:?}");
        assert!(err.contains("may only contain ASCII"), "{err:?}");
    }
}
