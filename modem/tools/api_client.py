#!/usr/bin/env python3
"""A tiny client for the Cairn native API (SPEC 13.2, docs/api.md).

Standard library only. Connects over TCP (default 127.0.0.1:8002) or a Unix
socket, prints the hello and every event, and optionally runs a few
commands first:

    python3 modem/tools/api_client.py                          # watch events
    python3 modem/tools/api_client.py --events heard,session   # only some events
    python3 modem/tools/api_client.py --status --heard --seconds 0
    python3 modem/tools/api_client.py --connect N0CALL-2 --send "hello" --disconnect
    python3 modem/tools/api_client.py --broadcast "QST de N0CALL-1" --seconds 10
    python3 modem/tools/api_client.py --unix /run/cairn/api.sock --json

With --send/--file it waits until each send is delivered (or undelivered)
before --disconnect; --seconds limits how long it keeps printing events
afterwards (default: until Ctrl-C).
"""

from __future__ import annotations

import argparse
import json
import socket
import sys
import time

SUMMARY_KEYS = {
    "rx": ("from", "mode", "pdu_type", "bytes", "text"),
    "session": ("kind", "peer", "state", "reason", "ref"),
    "link": ("kind", "peer", "target", "snr_db", "frame_mcs", "bler", "answered"),
    "heard": ("address", "frame_type", "dst", "snr_db", "mcs", "width", "count"),
    "channel": ("dcd", "keyed", "holder", "holder_address", "turn", "waiting", "next_yield_s"),
    "dcd": ("raw", "detectors"),
    "tx": ("kind", "frame_type", "dst", "mcs", "keyed_s", "refs"),
    "error": ("severity", "code", "message", "ref"),
}


class Client:
    def __init__(self, args: argparse.Namespace) -> None:
        if args.unix:
            self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.sock.connect(args.unix)
        else:
            host, _, port = args.tcp.rpartition(":")
            self.sock = socket.create_connection((host or "127.0.0.1", int(port)))
        self.reader = self.sock.makefile("r", encoding="utf-8", newline="\n")
        self.json = args.json
        self.next_id = 1
        self.events: list[dict] = []

    def show(self, msg: dict) -> None:
        if self.json:
            print(json.dumps(msg, separators=(",", ":")), flush=True)
            return
        if "event" in msg:
            name = msg["event"]
            if name == "hello":
                text = f"{msg['station']} api {msg['api_version']} ({msg['implementation']['name']}), unsupported: {msg['unsupported']}"
            else:
                keys = SUMMARY_KEYS.get(name, ())
                text = " ".join(f"{k}={json.dumps(msg[k])}" for k in keys if k in msg and msg[k] is not None)
            print(f"{msg.get('time', '')} {name:8} {text}", flush=True)
        elif msg.get("ok"):
            rest = {k: v for k, v in msg.items() if k not in ("id", "ok", "cmd")}
            print(f"  -> {msg['cmd']} ok {json.dumps(rest)}", flush=True)
        else:
            print(f"  -> {msg.get('cmd')} failed: {msg['error']['code']}: {msg['error']['message']}", flush=True)

    def read(self) -> dict | None:
        line = self.reader.readline()
        if not line:
            return None
        msg = json.loads(line)
        if "event" in msg:
            self.events.append(msg)
        self.show(msg)
        return msg

    def request(self, cmd: str, **members) -> dict:
        req = {"id": self.next_id, "cmd": cmd, **members}
        self.next_id += 1
        self.sock.sendall((json.dumps(req) + "\n").encode())
        while True:
            msg = self.read()
            if msg is None:
                raise SystemExit("modem closed the connection")
            if "ok" in msg and msg.get("id") == req["id"]:
                return msg

    def wait_for(self, pred, timeout: float) -> bool:
        """Read events until pred(event) holds for one (seen ones included)."""
        if any(pred(e) for e in self.events):
            return True
        self.sock.settimeout(timeout)
        try:
            while True:
                msg = self.read()
                if msg is None:
                    return False
                if "event" in msg and pred(msg):
                    return True
        except (TimeoutError, socket.timeout):
            return False
        finally:
            self.sock.settimeout(None)


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--tcp", default="127.0.0.1:8002", help="HOST:PORT (default 127.0.0.1:8002)")
    p.add_argument("--unix", help="Unix socket path instead of TCP")
    p.add_argument("--events", help="comma-separated events to subscribe to (default all)")
    p.add_argument("--status", action="store_true", help="print status first")
    p.add_argument("--heard", action="store_true", help="print the heard list first")
    p.add_argument("--connect", metavar="CALL", help="open a session")
    p.add_argument("--send", metavar="TEXT", action="append", default=[], help="send text in the session")
    p.add_argument("--file", metavar="PATH", action="append", default=[], help="send a file in the session")
    p.add_argument("--broadcast", metavar="TEXT", action="append", default=[], help="send text as a UI broadcast")
    p.add_argument("--disconnect", action="store_true", help="disconnect when the sends are delivered")
    p.add_argument("--timeout", type=float, default=300.0, help="seconds to wait for each step")
    p.add_argument("--seconds", type=float, help="keep printing events this long (default: until Ctrl-C)")
    p.add_argument("--json", action="store_true", help="print raw JSON lines")
    args = p.parse_args()

    c = Client(args)
    hello = c.read()
    if not hello or hello.get("event") != "hello":
        print("no hello from the modem", file=sys.stderr)
        return 1
    if args.events:
        c.request("subscribe", events=[e.strip() for e in args.events.split(",") if e.strip()])
    if args.status:
        c.request("status")
    if args.heard:
        c.request("heard")
    if args.connect:
        r = c.request("connect", peer=args.connect)
        if not r["ok"]:
            return 1
        peer = r["peer"]
        if not c.wait_for(lambda e: e["event"] == "session" and e.get("state") in ("open", "failed"), args.timeout):
            print("no answer", file=sys.stderr)
            return 1
        if not any(e["event"] == "session" and e.get("state") == "open" and e["peer"] == peer for e in c.events):
            return 1
    refs = []
    for text in args.send:
        r = c.request("send", text=text)
        if r["ok"]:
            refs.append(r["ref"])
    for path in args.file:
        r = c.request("send", file=path)
        if r["ok"]:
            refs.append(r["ref"])
    for text in args.broadcast:
        c.request("send", mode="broadcast", text=text)
    for ref in refs:

        def done(e: dict, ref: int = ref) -> bool:
            return e["event"] == "session" and (e.get("ref") == ref or ref in e.get("refs", []))

        if not c.wait_for(done, args.timeout):
            print(f"send {ref}: no result within {args.timeout} s", file=sys.stderr)
    if args.disconnect:
        c.request("disconnect")
        c.wait_for(lambda e: e["event"] == "session" and e.get("state") in ("disconnected", "link_lost"), args.timeout)
    end = None if args.seconds is None else time.monotonic() + args.seconds
    try:
        while end is None or time.monotonic() < end:
            if end is not None:
                c.sock.settimeout(max(0.01, end - time.monotonic()))
            try:
                if c.read() is None:
                    break
            except (TimeoutError, socket.timeout):
                break
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
