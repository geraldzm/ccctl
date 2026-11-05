# Claude Code integration surface used by ccctl

Verified against **Claude Code 2.1.318** (Linux, Nov 2026). Everything here is
observed behaviour or read from string literals in the shipped binary, not a
stable public API. Re-check this file when `claude --version` changes
(`ccctl doctor` prints both the installed version and the tested one).

## 1. Discovery

| Source | What ccctl takes from it |
|---|---|
| `claude agents --json` | **Source of truth** for which sessions exist: `pid, cwd, kind, startedAt, sessionId, name, status`, and `waitingFor` while a session is `waiting` ([agent view docs](https://code.claude.com/docs/en/agent-view): "use `waitingFor` to distinguish a permission prompt from a session that needs your input") |
| `~/.claude/sessions/<pid>.json` | Fields the CLI does not expose: `messagingSocketPath`, `tmux` (e.g. `"0:@3.%7"`), `procStart`, `statusUpdatedAt`, `updatedAt`, `version` |

* Registry entries are merged into agents output by `pid`. If the registry `sessionId`
  differs from the agents one, the registry entry is ignored (pid reuse).
* With `discovery = "registry"` (or when `claude agents` fails) ccctl reads the
  registry alone and applies Claude's own liveness rule: `kill(pid, 0)` succeeds **and**
  `procStart` equals field 22 of `/proc/<pid>/stat` (start time in clock ticks).
* `wait`/`ask` poll the single registry file, not `claude agents`, so polling is cheap.

### Status mapping

The registry status enum in 2.1.318 is `busy | shell | idle | waiting`
(`UMe()` in the binary: `waiting` whenever a dialog is on top: permission
prompt, elicitation, worker/sandbox request; otherwise `busy` while loading).

| raw | ccctl |
|---|---|
| `idle` | `idle` |
| `busy`, `shell` (a `!` command running) | `busy` |
| `waiting` | `waiting_permission` |
| anything else | `unknown` |

`waiting` alone does not say *why*. `status`, `pending` and `list --json` report
`waiting_for`:

1. Claude's own `waitingFor`, from `claude agents --json` (or the registry file if it
   carries one). Any value containing "permission" is normalised to `permission`;
   other values pass through verbatim, e.g. `input needed`. The raw value is also
   exposed (`waiting_for_raw` in `pending`, `waiting_for` in `list --json`).
2. Otherwise the pane scrape's classification: `permission` (Yes/No options) or
   `question`.

`approve`/`deny` still require the on-screen dialog to parse as a Yes/No permission
prompt, because they need the option labels, which only exist on screen.

## 2. tmux target resolution

1. `tmux list-panes -a -F '#{pane_id} #{pane_pid} …'`
2. Walk the Claude pid's parent chain via `/proc/<pid>/stat` until a `pane_pid` matches
   (`matched_by: pid-ancestry`). No pane indexes are hardcoded.
3. Fallback: the registry `tmux` hint's `%pane_id`, accepted **only** if that pane's
   `pane_current_path` equals the session cwd (`matched_by: registry-hint+cwd`).

## 3. Inbox socket (cross-session messaging, `peerProtocol: 1`)

* Path: registry `messagingSocketPath`, normally `$XDG_RUNTIME_DIR/cc-socks/<pid>.sock`
  (fallbacks `/tmp/cc-socks/`, `/tmp/cc-socks-<uid>/`). Dir `0700`, socket `0600`.
  ccctl refuses sockets outside the configured roots, non-sockets, and sockets not
  owned by the caller's uid.
* Framing: `AF_UNIX`/`SOCK_STREAM`, NDJSON. The server closes a connection with no
  complete line after 30 s, so ccctl connects only once the body is ready.
* Auth line `{"type":"auth","token":…}` is **optional on Linux/macOS**; ccctl does not
  send one (the peer token lives in `~/.claude/sessions/<pid>.<hash>.key`, and using it
  would add nothing on Unix).
* Frame ccctl sends:

  ```json
  {"type":"user","message":{"role":"user","content":"<text>"},
   "msgV":1,"msg_id":"<uuid v4>","priority":"next"}
  ```

  - `content` **must be a string**. The binary logs *"Ignoring user message with
    missing or non-string content"* otherwise.
  - `msg_id` **must be a UUID**. The receiver validates it with a UUID regex (`uMt`)
    and records it as `origin.msg_id`. The `cc-msg-<32hex>` form described in older
    community notes (cc-peer, 2.1.234) is silently dropped from `origin` by 2.1.318.
  - `priority`: `now | next | later` (`--priority`).
  - `from` (reply address `uds:<percent-encoded path>`) is sent only with
    `--peer-receipt`; see §5.
* The receiver enqueues the message with `skipSlashCommands: true, isMeta: true`, so
  a body of `/goal …` is **plain text, never a command**. This is why `cmd` exists.
* In the transcript the delivered message appears as a `user` record with
  `origin: {kind:"peer", msg_id, …}` and content
  `"Another Claude session sent a message:\n…"` (when busy, a
  `queue-operation`/`enqueue` record comes first).

## 4. Transcript

* Path: `<projects_dir>/<cwd.replace(/[^a-zA-Z0-9]/g,"-")>/<sessionId>.jsonl`. The
  replacement runs per UTF-16 unit, so non-BMP characters become two dashes. If the
  sanitized name is longer than 200 chars, Claude keeps 200 and appends
  `-<base36 hash>`. ccctl then lists only dirs with that 200-char prefix and picks the
  one holding `<sessionId>.jsonl`. It never runs find or grep.
* Turn segmentation (`read`, `ask`): a new turn starts at a non-sidechain `user` record
  that is not a tool result and either has `origin.kind ∈ {human, peer,
  task-notification}` or is a non-meta prompt (slash-command records included,
  `<local-command-stdout>` excluded). A turn's text is the concatenation of its
  assistant `text` blocks. `complete` means the last assistant message's `stop_reason`
  is not `tool_use`.
* IDs accepted by `read --since`: assistant `message.id` (`msg_…`), record `uuid`,
  prompt record `uuid`, and the inbox `msg_id` printed by `send`.

## 5. Delivery receipts

The protocol has receipts (`{"type":"control","action":"peer_message_status",
"status":"held|delivered|denied|expired","orig_msg_id":…}` sent to the `from`
address), but **only for messages the receiver holds for approval**. An ordinary
delivery produces no receipt. So ccctl:

1. by default confirms through the transcript. After writing the frame it tails the
   target transcript from the pre-send offset for a `user` record with
   `origin.msg_id == msg_id` (`delivered`), or an `enqueue` with the same text
   (`queued`, meaning the session is busy). `--require-confirm` turns "not
   confirmed" into exit 7.
2. with `--peer-receipt` additionally binds `<sockdir>/<ccctl pid>-<8hex>.sock`, a name
   that satisfies the receiver's `^\d+(-[0-9a-f]{8})?\.sock$` reply-target check. It
   sends that as `from` and waits `timeouts.peer_receipt` seconds for a
   `peer_message_status`. This is off by default because a `from` makes the receiving
   Claude believe it can reply, and the socket is gone moments later.

## 6. Permission prompts

The option labels exist only on screen. ccctl captures the pane, finds the bottom-most
`1. … 2. … 3. …` block (box-drawing characters stripped, wrapped labels re-joined) and
classifies it as `permission` when there is a `Yes…` option and a `No…` option.
Options are chosen by label:

| flag | label rule (2.1.318 wording) |
|---|---|
| `--once` | starts with "Yes" and has no "don't ask"/"always"/"session"/", and" |
| `--session` | "Yes…" containing "session" (e.g. *Yes, allow all edits during this session*) |
| `--always` | "Yes…" containing "don't ask again" / "always" |
| `deny` | starts with "No" (fallback: Escape) |

Keys are sent the way a person would: arrow keys from the currently highlighted option
(`❯`) to the target, then Enter, one key per `send-keys`. The screen is re-captured
just before acting, and ccctl refuses if the dialog changed. If no option matches,
ccctl exits 9 and prints the options it saw. It never guesses.

## 7. Known gaps / things the protocol does not offer

* **Socket messages are not user prompts.** They arrive wrapped as a message from
  another session. Claude Code tells the receiving model that such messages cannot
  grant approval or change configuration, and it never runs slash commands from them.
  To send something with full user authority, use `send --via tmux` (bracketed paste +
  Enter), or `cmd` for commands.
* **Bypass-permissions sessions hold socket messages.** With no `crossSessionInbound`
  setting, a session in bypass mode holds unsolicited messages behind an approval
  dialog (status becomes `waiting`). Options: approve it in the pane, use
  `--via tmux`, or set `"crossSessionInbound": "accept"` in that session's settings.
* **No receipt for normal delivery** (see §5). Transcript confirmation fills the gap.
* **`waitingFor` is coarse.** It says *that* a permission is pending but not which tool
  or which options. The pane scrape is the only source for those. Its exact
  vocabulary is undocumented: ccctl only checks for "permission" and otherwise passes
  it through. None of the live sessions were waiting while this was written, so the
  field is exercised by the fake session only.
* **Stale idle after input.** The registry may still say `idle` for a moment after
  input arrives, and local commands (e.g. bare `/goal`) may never flip to `busy`.
  `wait --for idle` therefore ignores an idle older than ccctl's last send/cmd until
  `settle_ms` passes (state in `~/.local/state/ccctl/last-action/`).
* **Not used (future options):** the binary also handles `notify_when_idle` /
  `peer_idle_notice` frames (fields `action, from, msg_id, from_mode` and
  `orig_msg_id, state, finished_at, detail`; the exact envelope is unverified). They
  could replace polling for `wait --for idle`, but they need a reply socket that
  outlives the command.
