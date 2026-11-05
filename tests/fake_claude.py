#!/usr/bin/env python3
"""Fake Claude Code session for ccctl end-to-end tests.

Mimics just enough of Claude Code 2.1.x to exercise ccctl without a real
session:
  * writes a registry file <sessions>/<pid>.json (status idle|busy|waiting)
  * listens on an NDJSON inbox socket and records peer messages in the
    transcript with origin.kind=peer / origin.msg_id, then "replies"
  * reads keystrokes from the tty (cbreak); Enter submits the typed line.
    "/perm" shows a permission dialog navigable with Up/Down/Enter/Esc and
    writes the chosen label to <state>/choice.txt
Usage: fake_claude.py <sessions_dir> <projects_dir> <sock_dir> <cwd> <name> <state_dir>
"""
import json, os, socket, sys, termios, threading, time, tty, uuid, re

sessions, projects, sockdir, cwd, name, statedir = sys.argv[1:7]
pid = os.getpid()
sid = str(uuid.uuid4())
sock_path = os.path.join(sockdir, f"{pid}.sock")
proc_start = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()[19]
tdir = os.path.join(projects, re.sub(r"[^a-zA-Z0-9]", "-", cwd))
os.makedirs(tdir, exist_ok=True)
tpath = os.path.join(tdir, f"{sid}.jsonl")
lock = threading.Lock()
now = lambda: int(time.time() * 1000)


def reg(status):
    rec = {"pid": pid, "sessionId": sid, "cwd": cwd, "startedAt": now(), "procStart": proc_start,
           "version": "fake", "peerProtocol": 1, "kind": "interactive", "entrypoint": "cli",
           "messagingSocketPath": sock_path, "name": name, "status": status,
           "updatedAt": now(), "statusUpdatedAt": now()}
    if status == "waiting":
        rec["waitingFor"] = "permission prompt"
    tmp = os.path.join(sessions, f".{pid}.tmp")
    with open(tmp, "w") as f:
        json.dump(rec, f)
    os.replace(tmp, os.path.join(sessions, f"{pid}.json"))


def append(rec):
    rec.setdefault("timestamp", time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime()))
    rec.setdefault("uuid", str(uuid.uuid4()))
    rec.setdefault("isSidechain", False)
    with lock, open(tpath, "a") as f:
        f.write(json.dumps(rec) + "\n")


def turn(user_content, origin, reply):
    reg("busy")
    append({"type": "user", "origin": origin, "message": {"role": "user", "content": user_content}})
    time.sleep(0.6)
    mid = "msg_" + uuid.uuid4().hex[:20]
    append({"type": "assistant", "message": {"id": mid, "role": "assistant",
            "content": [{"type": "text", "text": reply}], "stop_reason": "end_turn"}})
    reg("idle")


def inbox():
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(sock_path)
    os.chmod(sock_path, 0o600)
    srv.listen(8)
    while True:
        conn, _ = srv.accept()
        data = b""
        conn.settimeout(2)
        try:
            while not data.endswith(b"\n"):
                chunk = conn.recv(65536)
                if not chunk:
                    break
                data += chunk
        except Exception:
            pass
        conn.close()
        for line in data.decode().splitlines():
            try:
                fr = json.loads(line)
            except Exception:
                continue
            if fr.get("type") != "user" or not isinstance(fr["message"].get("content"), str):
                continue
            body = fr["message"]["content"]
            with open(os.path.join(statedir, "inbox.log"), "a") as f:
                f.write(line + "\n")
            threading.Thread(target=turn, args=(
                "Another Claude session sent a message:\n" + body,
                {"kind": "peer", "msg_id": fr.get("msg_id")},
                "echo: " + body), daemon=True).start()


OPTIONS = ["Yes", "Yes, and don't ask again for rm commands in " + cwd,
           "No, and tell Claude what to do differently (esc)"]


def draw_dialog(sel):
    sys.stdout.write("\x1b[2J\x1b[H")
    sys.stdout.write("Bash command\n\n  rm -rf build/\n\nDo you want to proceed?\n")
    for i, o in enumerate(OPTIONS):
        sys.stdout.write(("❯ " if i == sel else "  ") + f"{i + 1}. {o}\n")
    sys.stdout.flush()


def draw_prompt(buf):
    sys.stdout.write("\x1b[2J\x1b[H" + f"fake claude {name}\n> {buf}")
    sys.stdout.flush()


def main():
    os.makedirs(sessions, exist_ok=True)
    open(tpath, "a").close()
    reg("idle")
    threading.Thread(target=inbox, daemon=True).start()
    tty.setcbreak(sys.stdin.fileno())
    attrs = termios.tcgetattr(0)
    attrs[3] &= ~termios.ISIG  # deliver Ctrl-C as a byte, like Claude's TUI
    termios.tcsetattr(0, termios.TCSANOW, attrs)
    sys.stdout.write("\x1b[?2004h")  # enable bracketed paste
    buf, dialog, sel, pasting = "", False, 0, False
    draw_prompt(buf)
    while True:
        ch = os.read(0, 4096).decode(errors="replace")
        if pasting or "\x1b[200~" in ch:
            pre, _, rest = ch.partition("\x1b[200~") if not pasting else ("", "", ch)
            buf += pre
            body, end, after = rest.partition("\x1b[201~")
            buf += body.replace("\r", "\n")
            pasting = not end
            if not after:
                draw_prompt(buf)
                continue
            ch = after
        if dialog:
            keys = re.findall(r"\x1b\[[AB]|\x1bO[AB]|\x1b|\r|\n|.", ch)
            for k in keys:
                if not dialog:
                    break
                if k in ("\x1b[B", "\x1bOB"):
                    sel = min(sel + 1, len(OPTIONS) - 1)
                elif k in ("\x1b[A", "\x1bOA"):
                    sel = max(sel - 1, 0)
                elif k in ("\r", "\n", "\x1b"):
                    choice = OPTIONS[sel] if k != "\x1b" else "ESC"
                    with open(os.path.join(statedir, "choice.txt"), "w") as f:
                        f.write(choice)
                    dialog = False
                    reg("idle")
                    draw_prompt("")
            if dialog:
                draw_dialog(sel)
            continue
        if ch in ("\r", "\n"):
            line, buf = buf, ""
            with open(os.path.join(statedir, "typed.log"), "a") as f:
                f.write(line + "\n")
            if line == "/perm":
                dialog, sel = True, 0
                reg("waiting")
                draw_dialog(sel)
                continue
            if line.startswith("/"):
                append({"type": "user", "message": {"role": "user",
                        "content": f"<command-name>{line}</command-name>"}})
            else:
                threading.Thread(target=turn, args=(line, {"kind": "human"}, "typed: " + line),
                                 daemon=True).start()
            draw_prompt(buf)
        elif ch == "\x03":
            with open(os.path.join(statedir, "typed.log"), "a") as f:
                f.write("<C-c>\n")
        else:
            buf += ch
            draw_prompt(buf)


if __name__ == "__main__":
    main()
