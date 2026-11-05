//! ccctl — control the Claude Code sessions already living in your tmux panes.

mod cli;
mod commands;
mod config;
mod discovery;
mod error;
mod inbox;
mod pane;
mod state;
mod tmux;
mod transcript;
mod util;

use clap::Parser;
use cli::{Cli, Cmd};
use commands::Ctx;
use config::Config;
use error::Code;

/// Claude Code version the protocol notes and parsers were verified against.
pub const TESTED_CLAUDE_VERSION: &str = "2.1.318";

fn main() {
    let cli = Cli::parse();
    let path = cli.config.clone().unwrap_or_else(Config::default_path);
    let result = Config::load(&path).and_then(|cfg| {
        let ctx = Ctx::new(cfg, path, cli.json);
        match &cli.cmd {
            Cmd::List { all } => commands::list(&ctx, *all),
            Cmd::Status { session } => commands::status(&ctx, session),
            Cmd::Send(a) => commands::send(&ctx, a),
            Cmd::Cmd(a) => commands::cmd(&ctx, a),
            Cmd::Ask(a) => commands::ask(&ctx, a),
            Cmd::Wait(a) => commands::wait(&ctx, a),
            Cmd::Read(a) => commands::read(&ctx, a),
            Cmd::Pending => commands::pending(&ctx),
            Cmd::Approve(a) => commands::approve(&ctx, a),
            Cmd::Deny(g) => commands::deny(&ctx, g),
            Cmd::Attach { session } => commands::attach(&ctx, session),
            Cmd::Doctor => commands::doctor(&ctx),
        }
    });
    if let Err(e) = result {
        eprintln!("ccctl: error: {}", e.msg);
        if cli.json && e.code != Code::Timeout {
            // Keep stdout parseable for scripts: one JSON error object.
            println!(
                "{}",
                serde_json::json!({"error": e.msg, "code": e.code as i32})
            );
        }
        std::process::exit(e.code as i32);
    }
}
