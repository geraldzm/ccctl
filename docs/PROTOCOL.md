# Claude Code integration surface used by ccctl

Verified against **Claude Code 2.1.318**; dialog handling re-verified against
**2.1.328** (Linux, Nov 2026). Everything here is
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

`waiting` alone does not say *why*. Claude Code 2.1.328 derives `waitingFor` from the
dialog on top (`YQ()`/`H4o()` and the per-dialog table `Rf` in the binary):

| `waitingFor` | When | ccctl `waiting_for` |
|---|---|---|
| `permission prompt` | default for any dialog kind not in the table: every tool-permission request (Bash, Edit, WebFetch, Task/Agent, …) | `permission` |
| `input needed` | `permission_ask_user_question` (Claude's AskUserQuestion), MCP elicitations (`mcp_elicitation`, `mcp_url_elicitation`), a few setup questions | `question` |
| `dialog open` | other modal dialogs (local JSX commands, notices, mode proposals, …) | `dialog` |
| `worker request`, `sandbox request`, `goal proposal` | as named | passed through |

`status`, `pending` and `list --json` report `waiting_for` from that value
(`waiting_for_raw` keeps the original). Without one, ccctl uses the screen read
(`permission` / `question`), or `unknown` if nothing on screen was recognized.

**"input needed" is not a tool permission.** It is almost always Claude asking you a
multiple-choice question. `approve` refuses it and points at `choose`. That was the
root cause of the Nov 2026 reports where sessions showed "input needed" and
`approve` said "could not find a dialog".

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

## 6. Dialogs on screen (permission prompts, questions)

Option labels exist only on screen. While a session is `waiting`, ccctl captures the
pane (`tmux capture-pane -p -J`, visible screen only, read-only) and looks for the
**bottom-most live menu**:

* **Numbered menus** (`❯ 1. Yes`, `2) No`, `3. [ ] Option`): a chain `1..k`, k ≥ 2,
  read upward from the bottom option. Up to 8 lines are allowed between consecutive
  options. Those lines are wrapped label text (joined into the label when the next line
  starts lowercase, `/`, `(`…) or descriptions (kept as `detail`, as in
  AskUserQuestion). Box drawing, `────` rules and footer hints (`Enter to select ·
  Esc to cancel`) are skipped.
* **Menus without indexes** (`hideIndexes`): a `❯` cursor line plus contiguous
  siblings aligned at the same text column (2–9 options).
* **It must be a live select, not prose.** Exactly one option carries the `❯` cursor.
  At most 10 non-empty lines may follow the menu, and the prompt input box (a `❯`
  line right under a rule, including titled rules like `──── session-name ─`) must
  not appear below it. Claude hides the input box while a dialog is open, so a
  numbered list in a reply above the input box is rejected.
* **Notices** such as *"This terminal is too short. Make it taller to choose Yes, or
  choose No."* are reported as `notice`. `approve` refuses to act and asks for a taller
  pane, because ccctl never resizes your layout.
* **Unrecognized dialogs**: whenever the session waits but no menu is found (or a
  notice is shown), `status`/`pending` include `screen`, the raw bottom 40 lines, and
  `dialog_recognized: false`. `approve`/`choose` exit 9 and print that screen. `deny`
  presses Escape only with `--force`.

A menu is classified `permission` when it has a `Yes…` and a `No…` option. Actions:

| command | on a permission prompt | on a question / other menu |
|---|---|---|
| `approve --once` | "Yes…" with no "don't ask"/"always"/"session"/", and" | refused (exit 9), points at `choose` |
| `approve --session` | "Yes…" containing "session" (e.g. *Yes, allow all edits during this session*) | refused |
| `approve --always` | "Yes…" containing "don't ask again" / "always" | refused |
| `deny` | the "No…" option (fallback Escape) | Escape (cancel) |
| `choose <n\|label>` | any option | any option (number, exact label, unique prefix, unique substring) |

`approve` also refuses when Claude's own `waitingFor` says the dialog is not a
`permission prompt`, even if the labels look like Yes/No.

Keys are sent the way a person would: arrow keys from the highlighted option (`❯`) to
the target, then Enter, one key per `send-keys` with an 80 ms gap. The screen is
re-captured right before acting, and ccctl refuses if the menu changed. Afterwards it
waits up to 3 s for the menu to go away and reports `dialog_cleared`.

Regression fixtures live in `tests/fixtures/dialogs/`. To add one from a live session
(read-only): `ccctl status <s> --screen --json | jq -r .screen >
tests/fixtures/dialogs/<name>.txt`, then strip anything private before committing.

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
* **`waitingFor` is coarse.** It names the dialog family (§1) but not the tool, the
  question or the options. The screen read is the only source for those, and Claude's
  dialog layouts change between releases. When a new layout isn't recognized, ccctl
  reports it (`dialog_recognized: false` plus the raw `screen`) instead of guessing.
  Capture it as a fixture (§6).
* **Multi-select questions** (checkbox menus with a separate Submit step) can be read,
  and `choose` moves to and presses Enter on an option. Toggling several boxes and
  submitting still needs `cmd --keys` (Space/Enter/Tab) or the pane.
* **Stale idle after input.** The registry may still say `idle` for a moment after
  input arrives, and local commands (e.g. bare `/goal`) may never flip to `busy`.
  `wait --for idle` therefore ignores an idle older than ccctl's last send/cmd until
  `settle_ms` passes (state in `~/.local/state/ccctl/last-action/`).
* **Not used (future options):** the binary also handles `notify_when_idle` /
  `peer_idle_notice` frames (fields `action, from, msg_id, from_mode` and
  `orig_msg_id, state, finished_at, detail`; the exact envelope is unverified). They
  could replace polling for `wait --for idle`, but they need a reply socket that
  outlives the command.
