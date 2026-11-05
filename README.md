# ccctl

**The CLI tool to control your Claude Code sessions running in tmux.**

You already run Claude Code in tmux panes, next to your editor and terminals. ccctl
lets you, or your scripts, drive those sessions from any shell:

- find them,
- message them,
- run their slash commands,
- wait until they finish,
- read their replies,
- answer their permission prompts.

It adopts the sessions you already have. It never spawns its own sessions, never
rearranges your layout, and only attaches to a pane when you ask.

```
$ ccctl list
NAME          STATUS              PID  TMUX        SESSION ID                            CWD
api-worker    busy               5678  0:2.1 %2    3b9e2c41-7d5a-4f10-9c3e-2a8f6d1b0e57  /home/me/src/api
docs-writer   waiting_permission 1234  0:1.1 %0    8c41f0d2-5e6b-4a7c-b1d9-6f20e3a4c815  /home/me/src/docs

$ ccctl ask api-worker --text "Did the migration tests pass?"
Yes. All 42 migration tests pass on the current branch.

$ ccctl approve docs-writer --once
```

Under the hood it uses the interfaces Claude Code itself provides:

- **Discovery:** `claude agents --json` (agent view), including `waitingFor`.
- **Sending messages:** the per-session inbox socket.
- **Commands and keys:** the session's real tmux pane.
- **Replies:** the session transcript.

Tested against **Claude Code 2.1.318** on Linux with tmux 3.8. See
[docs/PROTOCOL.md](docs/PROTOCOL.md) for every protocol assumption and
[docs/SMOKE.md](docs/SMOKE.md) for the live checklist.

## Install

```sh
cargo install ccctl                          # from crates.io
# or from a checkout:
cargo install --path . --root ~/.local       # binary -> ~/.local/bin/ccctl

mkdir -p ~/.config/ccctl && cp -n config.example.toml ~/.config/ccctl/config.toml   # optional
ccctl doctor
```

Requirements: Rust ≥ 1.85, tmux, Claude Code ≥ 2.1.224 (cross-session inbox). The
tests also need python3.

## Day-one happy path

```sh
ccctl list --json
ccctl cmd  my-session -- "/goal"            # runs the TUI command
ccctl send my-session --file goal.txt       # chat message via the inbox socket
ccctl cmd  my-session -- "go"
ccctl wait my-session --for idle
ccctl read my-session --json
```

## Commands

Sessions are selected by **name**, **session id** (or a unique ≥4-char prefix of id or
name) or **pid**. Ambiguous selectors fail with exit 4 and list the candidates.

| Command | What it does |
|---|---|
| `list [--all] [--json]` | Interactive sessions: name, status, pid, cwd, tmux target, session id, socket. `--all` adds background sessions. |
| `status <s> [--json]` | One session plus last activity, transcript path, last msg id, and, if waiting, `waiting_for` and the on-screen dialog. |
| `send <s> --text T \| --file P \| --stdin` | Plain chat message. Confirms delivery from the transcript. Prints the inbox `msg_id`. |
| `cmd <s> -- "text"` | Types literal text into the pane, then Enter (`--no-enter` to skip). For `/commands` and TUI input. |
| `cmd <s> --keys K...` | Key presses: `Enter`, `Escape`/`esc`, `C-c`/`ctrl-c`, `Up`, `Down`, `BTab`/`shift-tab`, `Tab`, `1`… |
| `ask <s> --text T [--timeout S]` | send → wait until idle → print the reply turn (`--json` for the full turn object). |
| `wait <s> [--for idle\|busy\|waiting_permission] [--timeout S]` | Exit 0 on match, 6 on timeout. |
| `read <s> [--last N] [--since ID] [--json]` | Last N assistant turns from `~/.claude/projects/<cwd>/<sessionId>.jsonl`. |
| `pending [--json]` | Sessions blocked on a dialog: Claude's `waitingFor`, plus the question and options read from the pane. |
| `approve <s> [--once\|--session\|--always]` | Picks the matching option by label (arrows + Enter, as in the pane). Default `--once`. |
| `deny <s>` | Picks the "No…" option (Escape if none). |
| `attach <s>` | `switch-client` (inside tmux) or `attach-session` to that pane. Only on request. |
| `doctor [--json]` | Checks the environment: claude version, registry, sockets, tmux, config. |

Common `send`/`ask` options: `--via auto|socket|tmux` (default `auto` = socket if the
session has one, otherwise tmux), `--priority now|next|later`, `--confirm-timeout S`,
`--require-confirm` (exit 7 unless confirmed), `--peer-receipt`, `--allow-self`.

### Output and exit codes

* With `--json`, stdout carries exactly one JSON document (errors become
  `{"error":…, "code":N}`). Without it, stdout carries data (tables, reply text,
  msg_id) and every diagnostic goes to stderr.
* Exit codes are stable:

| code | meaning |
|---|---|
| 0 | ok |
| 1 | unexpected error |
| 2 | usage / bad config |
| 3 | session not found (or exited while waiting) |
| 4 | selector ambiguous |
| 5 | not allowed (allowlist, own session, or a dialog is on screen) |
| 6 | timeout |
| 7 | delivery failed / not confirmed with `--require-confirm` / held message denied |
| 8 | no tmux pane for the session, or pane in copy mode |
| 9 | no permission prompt (or no matching option) |
| 10 | transcript missing / `--since` id not found |
| 11 | discovery failed |

## `send` vs `cmd`: why there are two

* **`send`** goes through Claude Code's **inbox socket** (cross-session messaging). The
  receiver enqueues it with `skipSlashCommands`, so `/goal` inside a message is just
  text. Use `send` for prose, including prose that *mentions* `/some-skill`. It works
  while the session is busy (the message is read at the next tool boundary) and needs
  no tmux.
* **`cmd`** types into the **real tmux pane**, so the TUI's own command parser runs. Use
  it for slash commands (`/goal`, `/compact`, `/clear`), bare replies like `go`, and
  keys (`--keys esc`, `--keys ctrl-c`).

Caveat: a socket message reaches the model as *"Another Claude session sent a
message"*, with less authority than something you typed (it can't approve anything or
change config). When you need it to read exactly like your own typed prompt, use
`send --via tmux` (bracketed paste + Enter).

## Config

`~/.config/ccctl/config.toml` (override with `--config` or `$CCCTL_CONFIG`). All keys
are optional. See [config.example.toml](config.example.toml).

```toml
[security]
allowlist = ["api-*", "scratch"]   # globs; absent = all sessions, [] = none

[timeouts]
wait = 600.0
ask = 900.0
send_confirm = 10.0

[sockets]
roots = []            # default: $XDG_RUNTIME_DIR/cc-socks, /tmp/cc-socks, /tmp/cc-socks-<uid>

[claude]
discovery = "agents"  # or "registry" to skip spawning `claude agents --json`
```

Unknown keys are rejected so typos fail loudly (exit 2).

## Security model

* **Allowlist.** `send`, `cmd`, `ask`, `approve` and `deny` refuse sessions whose name
  matches no `[security].allowlist` glob (exit 5). Read-only commands are unaffected.
* **Self-protection.** ccctl refuses to drive the Claude session it is running inside
  (detected by walking its own process ancestry) unless you pass `--allow-self`. This
  stops an automation running in session A from typing into A.
* **Dialog guard.** Typed input (`cmd`, `send --via tmux`) is refused while the target
  shows a dialog, since keystrokes would answer it. Use `approve`/`deny`/`--keys` or
  `--force`.
* **Approvals** pick options by their on-screen label and re-read the screen just
  before acting. If the expected option is missing or the dialog changed, ccctl stops
  instead of guessing.
* **Sockets.** Only sockets inside the configured roots, of socket type, and owned by
  your uid are used. Everything stays local. Nothing goes through Anthropic servers.
* Same-uid processes can already do all of this (the inbox socket and tmux are
  per-user). The allowlist protects you from your own automations, not from other
  users.

## Development

```sh
cargo test             # unit tests + e2e against tests/fake_claude.py in a private tmux server
cargo clippy --all-targets
```

Layout: `discovery.rs` (agents + registry + liveness), `tmux.rs` (pid→pane, keys,
paste, capture), `inbox.rs` (socket frames, receipts), `transcript.rs` (path
derivation, turns, delivery detection), `pane.rs` (dialog parsing),
`commands.rs` (glue), `state.rs` (last-action time for `wait`).

When Claude Code updates: run `ccctl doctor`, then `cargo test`, then
[docs/SMOKE.md](docs/SMOKE.md), and bump `TESTED_CLAUDE_VERSION` in `src/main.rs` and
the version in docs/PROTOCOL.md.

## License

MIT. See [LICENSE](LICENSE).
