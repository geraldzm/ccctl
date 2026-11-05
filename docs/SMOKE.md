# Manual smoke checklist (live tmux + real Claude Code)

The automated suite (`cargo test`) covers everything against a fake session
(`tests/fake_claude.py` in a private tmux server). This checklist exercises
the parts only a real Claude Code TUI can confirm. Run it after upgrading
Claude Code and record the version at the bottom.

Use a **throwaway session**, not one doing real work:

```sh
# in a new tmux window
mkdir -p /tmp/ccctl-smoke && cd /tmp/ccctl-smoke && claude --name ccctl-smoke
```

Run the commands below from a different pane (not inside the smoke session).
Each step lists what to expect.

## Discovery

- [ ] `ccctl doctor`: claude version shown, registry live count > 0, discovery OK, tmux panes > 0.
- [ ] `ccctl list`: `ccctl-smoke` row is `idle` with a TMUX target matching the window you opened (e.g. `0:5.1 %12`).
- [ ] `ccctl list --json | jq '.[] | select(.name=="ccctl-smoke") | .tmux.matched_by'` prints `"pid-ancestry"`.
- [ ] `ccctl status ccctl-smoke`: socket path exists, and the transcript path exists once the session has written anything.

## Talk + wait + read (day-one happy path)

- [ ] `ccctl cmd ccctl-smoke -- "/goal"`: the pane shows `/goal` executed as a command (not as chat text).
- [ ] `printf 'Reply with exactly: PONG-1\n' > /tmp/ccctl-smoke/goal.txt && ccctl send ccctl-smoke --file /tmp/ccctl-smoke/goal.txt`: stderr says `delivered`, and stdout prints a UUID msg_id. The pane shows `› Message from …`.
- [ ] `ccctl cmd ccctl-smoke -- "go"`: typed as a normal prompt.
- [ ] `ccctl wait ccctl-smoke --for idle --timeout 300`: exit 0 once the session finishes. Does not return instantly.
- [ ] `ccctl read ccctl-smoke --json | jq -r '.turns[0].text'`: shows the latest reply.
- [ ] `ccctl read ccctl-smoke --last 3`: three turns with `── msg_… · time · complete` headers.

## ask / send variants

- [ ] `ccctl ask ccctl-smoke --text "Reply with exactly: PONG-2"`: prints `PONG-2`.
- [ ] `ccctl send ccctl-smoke --text "/compact"`: arrives as plain text; the session does **not** compact.
- [ ] `ccctl send ccctl-smoke --via tmux --text $'two\nlines'`: pasted as one multi-line prompt, state `delivered`.
- [ ] While the session is busy (give it a long task), `ccctl send …`: state `queued`, and the message is read at the next tool boundary.

## Keys

- [ ] During a long task: `ccctl cmd ccctl-smoke --keys esc`: interrupts like pressing Esc.
- [ ] `ccctl cmd ccctl-smoke --keys shift-tab`: permission mode indicator cycles.
- [ ] Put the pane in copy mode (`prefix [`), then `ccctl cmd ccctl-smoke -- hi`: refused (exit 8). With `--exit-copy-mode`: works.

## Approvals

(Make sure the smoke session is in the default permission mode.)

- [ ] `ccctl ask ccctl-smoke --text "Run the bash command: touch /tmp/ccctl-smoke/a" --timeout 5`: times out (exit 6) with a stderr note that it is waiting on a permission prompt.
- [ ] `ccctl pending`: lists `ccctl-smoke` with the Bash command, the question and numbered options.
- [ ] `ccctl status ccctl-smoke`: `waiting_permission`, prompt shown.
- [ ] `claude agents --json | jq '.[] | select(.name=="ccctl-smoke") | .waitingFor'`: note the exact value here (_____). `ccctl pending --json` shows the same value as `waiting_for_raw` and `waiting_for: "permission"`.
- [ ] `ccctl cmd ccctl-smoke -- hello`: refused (exit 5, "showing a dialog").
- [ ] `ccctl approve ccctl-smoke --once`: the dialog closes and the file is created.
- [ ] Repeat with `touch b` and `ccctl deny ccctl-smoke`: the dialog closes with "No".
- [ ] Repeat with a file edit, then `ccctl approve ccctl-smoke --session`: picks *Yes, allow all edits during this session*.
- [ ] `ccctl approve ccctl-smoke` with no dialog: exit 9.

## Safety

- [ ] Add `[security] allowlist = ["nothing"]` to the config. `ccctl send ccctl-smoke --text x` → exit 5. `ccctl read ccctl-smoke` still works.
- [ ] From inside a Claude session's Bash tool, `ccctl send <that same session> --text x` → exit 5 (self), unless `--allow-self` is passed.
- [ ] `ccctl attach ccctl-smoke` (inside tmux): switches your client to the smoke pane. Nothing else ever switches automatically.

## Teardown

- [ ] `/exit` in the smoke session. `ccctl list` no longer shows it, and `ccctl status ccctl-smoke` → exit 3.

---
Last run: _(date)_ · Claude Code _(version)_ · result _(pass/fail + notes)_
