mod client;
mod paths;
mod protocol;
mod sanitize;
mod server;
mod tmux_compat;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use client::AttachStdinEof;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use uuid::Uuid;

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
        /// Disable the blue lterm status bar while attached.
        #[arg(long)]
        no_status: bool,
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
        /// Disable the blue lterm status bar while attached.
        #[arg(long)]
        no_status: bool,
        /// Required shell command to run in the tmux-compatible session.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Attach to a persistent session or pane.
    #[command(visible_aliases = ["a", "resume"])]
    Attach {
        /// Session or pane target to attach.
        #[arg(default_value = "%0")]
        target: String,
        /// Disable the blue lterm status bar while attached.
        #[arg(long)]
        no_status: bool,
    },
    /// Attach to a session, creating it first when missing.
    #[command(name = "open", visible_alias = "attach-or-new")]
    AttachOrNew {
        /// Session or pane target to attach or create.
        #[arg(default_value = "main")]
        target: String,
        /// Disable the blue lterm status bar while attached.
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
    #[command(visible_alias = "processes")]
    Ps {
        /// Optional session or pane target to inspect.
        target: Option<String>,
        /// Print process rows as a JSON array for automation.
        #[arg(long)]
        json: bool,
    },
    /// Close a session or pane.
    #[command(name = "close", visible_alias = "kill")]
    Close {
        /// Session or pane target to close.
        target: String,
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
    },
    /// Stop the daemon and all sessions.
    Shutdown,
    /// Install the tmux compatibility shim and print the shim directory.
    InstallShim,
    /// Print shell exports for tmux compatibility.
    Env,
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
        /// Built-in, configured, or PATH-resolved custom profile name, e.g. claude, codex, gemini.
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
    /// Run Gemini CLI inside a tmux-compatible lterm session.
    Gemini {
        #[command(flatten)]
        launch: AgentLaunchOptions,
        /// Arguments forwarded to gemini; use `--` before args that look like lterm options.
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
            // Hidden no-op kept for callers that already pass `run --tmux`.
            // `--no-tmux` intentionally wins so old aliases can opt out later.
            tmux: _,
            no_tmux,
            no_status,
            command,
        } => {
            let tmux = !no_tmux;
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
        Commands::Close { target } => client::kill(&target),
        Commands::Input {
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
        Commands::Logs { target, start } => {
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
        Commands::Claude { launch, args } => {
            run_agent_profile(AgentProfile::known("claude"), launch, args)
        }
        Commands::Codex { launch, args } => {
            run_agent_profile(AgentProfile::known("codex"), launch, args)
        }
        Commands::Gemini { launch, args } => {
            run_agent_profile(AgentProfile::known("gemini"), launch, args)
        }
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
}

const BUILT_IN_AGENT_PROFILES: &[&str] = &["claude", "codex", "gemini", "omx", "omc"];

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
            "gemini" => Self {
                name: name.to_string(),
                binary: "gemini".to_string(),
                session_base: "gemini-lterm".to_string(),
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
            "gemini" => Ok(Self::known("gemini")),
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
        let claude = AgentProfile::resolve("claude").expect("claude profile");
        assert_eq!(claude.binary, "claude");
        assert_eq!(claude.session_base, "claude-lterm");
        assert!(!claude.show_status);

        let codex = AgentProfile::resolve("codex").expect("codex profile");
        assert_eq!(codex.binary, "codex");
        assert_eq!(codex.session_base, "codex-lterm");
        assert!(!codex.show_status);

        let gemini = AgentProfile::resolve("gemini").expect("gemini profile");
        assert_eq!(gemini.binary, "gemini");
        assert_eq!(gemini.session_base, "gemini-lterm");
        assert!(!gemini.show_status);

        let omx = AgentProfile::resolve("omx").expect("omx profile");
        assert_eq!(omx.binary, "omx");
        assert_eq!(omx.session_base, "omx-lterm");
        assert!(omx.show_status);
    }

    #[test]
    fn built_in_agent_profile_infos_match_launcher_contract() {
        let infos = built_in_agent_profile_infos();
        let names: Vec<_> = infos.iter().map(|info| info.profile.as_str()).collect();
        assert_eq!(names, ["claude", "codex", "gemini", "omx", "omc"]);

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
