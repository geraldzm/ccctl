//! Command-line definition (clap derive).

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "ccctl",
    version,
    about = "The CLI tool to control your Claude Code sessions running in tmux",
    long_about = "ccctl discovers running Claude Code sessions (`claude agents --json` + the \
session registry), resolves the tmux pane each one lives in from its pid, and lets you \
message them (inbox socket), drive their TUI (tmux keys), wait on their status, read \
their transcripts and answer permission prompts.\n\n\
Exit codes: 0 ok, 1 error, 2 usage, 3 not found, 4 ambiguous, 5 not allowed, 6 timeout, \
7 delivery failed, 8 no tmux pane, 9 no permission prompt, 10 transcript, 11 discovery."
)]
pub struct Cli {
    /// Config file (default: $CCCTL_CONFIG or ~/.config/ccctl/config.toml)
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Machine-readable JSON on stdout
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::enum_variant_names)]
pub enum Cmd {
    /// List interactive Claude sessions
    List {
        /// Include background/daemon sessions too
        #[arg(long)]
        all: bool,
    },
    /// Show one session (status, tmux target, transcript, last activity)
    Status { session: String },
    /// Send a normal chat message (inbox socket; never runs slash commands)
    Send(SendArgs),
    /// Type into the session's tmux pane so the TUI runs it (slash commands, keys)
    Cmd(CmdArgs),
    /// Send a message, wait until idle, print the assistant's reply
    Ask(AskArgs),
    /// Block until the session reaches a status
    Wait(WaitArgs),
    /// Print the last assistant turn(s) from the transcript
    Read(ReadArgs),
    /// List sessions waiting on a permission prompt / dialog
    Pending,
    /// Approve the permission prompt on screen
    Approve(ApproveArgs),
    /// Deny the permission prompt on screen
    Deny(GuardArgs),
    /// Switch to / attach the session's tmux pane (only when asked)
    Attach { session: String },
    /// Check environment: claude binary, registry, sockets, tmux, config
    Doctor,
}

#[derive(Args, Debug, Clone)]
pub struct GuardArgs {
    /// Session name, session id (or unique prefix), or pid
    pub session: String,
    /// Permit targeting the Claude session that ccctl itself is running under
    #[arg(long)]
    pub allow_self: bool,
}

#[derive(Args, Debug, Clone)]
#[command(group(ArgGroup::new("body").required(true).args(["text", "file", "stdin"])))]
pub struct Body {
    /// Message text
    #[arg(long, short = 't')]
    pub text: Option<String>,
    /// Read the message body from a file
    #[arg(long, short = 'f', value_name = "PATH")]
    pub file: Option<PathBuf>,
    /// Read the message body from stdin
    #[arg(long)]
    pub stdin: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// Inbox socket when the session has one, else tmux paste
    Auto,
    /// Inbox socket only (cross-session message)
    Socket,
    /// Bracketed paste into the tmux pane + Enter (arrives as a typed prompt)
    Tmux,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Now,
    Next,
    Later,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Now => "now",
            Priority::Next => "next",
            Priority::Later => "later",
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct DeliveryOpts {
    /// Delivery channel
    #[arg(long, value_enum, default_value_t = Via::Auto)]
    pub via: Via,
    /// Inbox queue priority
    #[arg(long, value_enum, default_value_t = Priority::Next)]
    pub priority: Priority,
    /// Seconds to watch the transcript for the delivery record (0 = don't)
    #[arg(long, value_name = "SEC")]
    pub confirm_timeout: Option<f64>,
    /// Exit 7 if delivery is not confirmed in time
    #[arg(long)]
    pub require_confirm: bool,
    /// Also bind a reply socket and wait for a peer_message_status receipt
    /// (only sent for messages the receiver holds for approval)
    #[arg(long)]
    pub peer_receipt: bool,
    /// Allow sending while the session shows a permission dialog (tmux only)
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone)]
pub struct SendArgs {
    #[command(flatten)]
    pub guard: GuardArgs,
    #[command(flatten)]
    pub body: Body,
    #[command(flatten)]
    pub delivery: DeliveryOpts,
}

#[derive(Args, Debug, Clone)]
#[command(group(ArgGroup::new("what").required(true).args(["keys", "input"])))]
pub struct CmdArgs {
    #[command(flatten)]
    pub guard: GuardArgs,
    /// Send key presses instead of text: tmux key names (Enter, C-c, Escape,
    /// Up, Down, BTab, Tab, 1..9) or aliases (ctrl-c, esc, shift-tab, return)
    #[arg(long, num_args = 1.., value_name = "KEY")]
    pub keys: Vec<String>,
    /// Type the text but do not press Enter
    #[arg(long)]
    pub no_enter: bool,
    /// If the pane is in copy/view mode, leave it first (otherwise refuse)
    #[arg(long)]
    pub exit_copy_mode: bool,
    /// Type even while a permission dialog is on screen
    #[arg(long)]
    pub force: bool,
    /// Text to type (joined with spaces), then Enter
    #[arg(last = true, num_args = 1..)]
    pub input: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct AskArgs {
    #[command(flatten)]
    pub guard: GuardArgs,
    #[command(flatten)]
    pub body: Body,
    #[command(flatten)]
    pub delivery: DeliveryOpts,
    /// Seconds to wait for the reply (default: config timeouts.ask)
    #[arg(long, value_name = "SEC")]
    pub timeout: Option<f64>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitFor {
    Idle,
    Busy,
    #[value(name = "waiting_permission", alias = "waiting")]
    WaitingPermission,
}

#[derive(Args, Debug, Clone)]
pub struct WaitArgs {
    pub session: String,
    /// Status to wait for
    #[arg(long = "for", value_enum, default_value_t = WaitFor::Idle)]
    pub for_: WaitFor,
    /// Seconds before giving up with exit 6 (default: config timeouts.wait)
    #[arg(long, value_name = "SEC")]
    pub timeout: Option<f64>,
    /// Override settle window after a ccctl send/cmd (ms)
    #[arg(long, value_name = "MS")]
    pub settle_ms: Option<u64>,
}

#[derive(Args, Debug, Clone)]
pub struct ReadArgs {
    pub session: String,
    /// Number of most recent assistant turns
    #[arg(long, default_value_t = 1)]
    pub last: usize,
    /// Only turns after the one containing this id (msg_…, record uuid, or
    /// the msg_id printed by `send`)
    #[arg(long, value_name = "ID")]
    pub since: Option<String>,
}

#[derive(Args, Debug, Clone)]
#[command(group(ArgGroup::new("scope").args(["once", "session_scope", "always"])))]
pub struct ApproveArgs {
    #[command(flatten)]
    pub guard: GuardArgs,
    /// Allow this one request (default)
    #[arg(long)]
    pub once: bool,
    /// Allow for the rest of this session ("… during this session")
    #[arg(long = "session")]
    pub session_scope: bool,
    /// Allow and don't ask again (persists an allow rule)
    #[arg(long)]
    pub always: bool,
}
