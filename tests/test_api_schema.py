"""The native API schema (docs/api.schema.json) is a valid JSON Schema, and
every example line in docs/api.md validates against it (SPEC 13.2)."""

from __future__ import annotations

import json
import socket
import subprocess
import sys
import threading
import re
from pathlib import Path

import jsonschema
import pytest

ROOT = Path(__file__).resolve().parents[1]
SCHEMA = json.loads((ROOT / "docs" / "api.schema.json").read_text())
DOC = (ROOT / "docs" / "api.md").read_text()


def examples() -> list[str]:
    lines = []
    for block in re.findall(r"```json\n(.*?)```", DOC, flags=re.S):
        lines.extend(ln for ln in block.splitlines() if ln.strip())
    return lines


def kind(obj: dict) -> str:
    if "cmd" in obj and "ok" not in obj:
        return "request"
    if "ok" in obj:
        return "response"
    return "event"


def validator(defname: str | None = None) -> jsonschema.Draft202012Validator:
    schema = SCHEMA if defname is None else {**SCHEMA, "oneOf": [{"$ref": f"#/$defs/{defname}"}]}
    return jsonschema.Draft202012Validator(schema)


def test_schema_is_valid() -> None:
    jsonschema.Draft202012Validator.check_schema(SCHEMA)


def test_doc_has_examples_of_every_command_and_event() -> None:
    objs = [json.loads(ln) for ln in examples()]
    cmds = {o["cmd"] for o in objs if kind(o) == "request"}
    events = {o["event"] for o in objs if kind(o) == "event"}
    assert cmds == set(SCHEMA["$defs"]["command_name"]["enum"])
    assert events == set(SCHEMA["$defs"]["event_name"]["enum"])


@pytest.mark.parametrize("line", examples())
def test_example_validates(line: str) -> None:
    obj = json.loads(line)
    validator(kind(obj)).validate(obj)
    validator().validate(obj)


@pytest.mark.parametrize(
    "bad",
    [
        {"cmd": "connect"},  # peer missing
        {"cmd": "send", "text": "a", "data": "YQ=="},  # two payloads
        {"cmd": "send", "text": "a", "priority": 4},
        {"cmd": "config", "set": {}},
        {"id": 1, "ok": True, "cmd": "send"},  # result members missing
        {"id": 1, "ok": False, "cmd": "send", "error": {"code": "nope", "message": ""}},
        {"event": "heard", "time": "2026-11-07T18:00:00Z", "modem_time": 1.0},  # time without ms
        {"event": "session", "time": "2026-11-07T18:00:00.000Z", "modem_time": 1.0, "kind": "state", "peer": "N0CALL"},
    ],
)
def test_bad_messages_fail(bad: dict) -> None:
    with pytest.raises(jsonschema.ValidationError):
        validator().validate(bad)


def test_example_client_speaks_the_schema() -> None:
    """modem/tools/api_client.py against a stand-in modem: every request it
    sends validates, and it follows a session to delivery and disconnect."""
    requests: list[dict] = []
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    port = srv.getsockname()[1]
    t0 = "2026-11-07T00:00:00.000Z"

    def ev(name: str, **m: object) -> dict:
        return {"event": name, "time": t0, "modem_time": 1.0, **m}

    def serve() -> None:
        conn, _ = srv.accept()
        f = conn.makefile("rw", encoding="utf-8", newline="\n")

        def send(obj: dict) -> None:
            validator().validate(obj)
            f.write(json.dumps(obj) + "\n")
            f.flush()

        send(
            ev(
                "hello",
                api="cairn",
                api_version="0.1.0",
                implementation={"name": "stand-in", "version": "0"},
                station="N0AAA",
                client=1,
                commands=SCHEMA["$defs"]["command_name"]["enum"],
                unsupported=["calibrate"],
                events=SCHEMA["$defs"]["event_name"]["enum"],
                encodings=["raw"],
            )
        )
        peer = None
        for line in f:
            req = json.loads(line)
            requests.append(req)
            rid, cmd = req.get("id"), req["cmd"]
            if cmd == "connect":
                peer = req["peer"].upper()
                send({"id": rid, "ok": True, "cmd": cmd, "peer": peer, "resuming": False})
                state = {"session_id": 7, "initiator": True, "resumed": False, "reason": None}
                send(ev("session", kind="state", peer=peer, state="open", **state))
            elif cmd == "send":
                send({"id": rid, "ok": True, "cmd": cmd, "ref": 1, "mode": "session", "bytes": 5, "pdus": 1,
                      "encoding": "raw"})
                send(ev("session", kind="delivered", peer=peer, ref=1, bytes=5))
            elif cmd == "disconnect":
                send({"id": rid, "ok": True, "cmd": cmd, "peer": peer, "graceful": True})
                state = {"session_id": 7, "initiator": None, "resumed": None, "reason": None}
                send(ev("session", kind="state", peer=peer, state="disconnected", **state))
            elif cmd == "status":
                send({"id": rid, "ok": False, "cmd": cmd, "error": {"code": "internal", "message": "stand-in"}})
            elif cmd == "subscribe":
                send({"id": rid, "ok": True, "cmd": cmd, "events": req["events"]})
        conn.close()

    th = threading.Thread(target=serve, daemon=True)
    th.start()
    client = ROOT / "modem" / "tools" / "api_client.py"
    args = ["--tcp", f"127.0.0.1:{port}", "--events", "session,rx", "--status", "--connect", "n0bbb"]
    args += ["--send", "hello", "--disconnect", "--seconds", "0", "--timeout", "10"]
    out = subprocess.run([sys.executable, str(client), *args], capture_output=True, text=True, timeout=60)
    assert out.returncode == 0, out.stderr
    assert [r["cmd"] for r in requests] == ["subscribe", "status", "connect", "send", "disconnect"]
    for r in requests:
        validator("request").validate(r)
    assert "delivered" in out.stdout and "disconnected" in out.stdout
