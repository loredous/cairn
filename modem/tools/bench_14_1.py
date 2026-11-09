#!/usr/bin/env python3
"""Measure the SPEC 14.1 24-tone targets on a pair of stations (v0.5 milestone, SPEC 16.2).

Standard library only. Drives `cairn-modem` (tx, listen, run, selftest) on station A (this
machine) and station B (this machine, or another over ssh), and writes results.json plus
results.md with a pass/fail table against each 14.1 row. See "Section 14.1 bench script" in
docs/bench-testing.md.

    # what would be sent, and by whom (keys nothing):
    python3 modem/tools/bench_14_1.py --config bench.toml --plan-only
    # on air (two stations you are licensed to operate; see the docs):
    python3 modem/tools/bench_14_1.py --config bench.toml --i-have-permission-to-transmit
    # the same procedure against the simulator (no hardware, no radio):
    python3 modem/tools/bench_14_1.py --simulate --quick
    # combine the results of several runs into the milestone table:
    python3 modem/tools/bench_14_1.py --merge runA/results.json runB/results.json

Rows that cannot be measured on air are listed in the results with the reason (NOT_MEASURABLE).
Simulation results are never evidence for the milestone: the file says so.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import shlex
import shutil
import socket
import statistics
import subprocess
import sys
import threading
import time
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

SCHEMA = "cairn-bench-14.1/1"
REPO = Path(__file__).resolve().parents[2]
DEFAULT_MODEM = REPO / "modem" / "target" / "release" / "cairn-modem"

# SPEC 6.2 thresholds (7 dB-equivalent audio SNR, dB) and Appendix A.2 (24 tones):
# data blocks per frame and net kbps, one direction, TXDelay 150 ms, ACK cycle included.
THRESHOLD_DB = [3.5, 6.0, 8.5, 12.5, 17.5, 21.0, 25.5]
A2_BLOCKS = [3, 4, 13, 20, 27, 34, 40]
A2_NET_KBPS = [1.21, 1.73, 3.02, 4.55, 6.09, 7.61, 9.11]
FRAME_AIR_S = [2.15, 2.15, 4.2, 4.2, 4.2, 4.2, 4.2]  # PTT on time with TXDelay 150 ms, to size listen windows

# Callsigns that are placeholders, not stations: refused on air (N0CALL is fine in simulation).
FICTITIOUS = {"N0CALL", "NOCALL", "MYCALL", "YOURCALL", "CALLSIGN", "TEST", "XXXXX", "NONE", "CALL"}
CALLSIGN_RE = re.compile(r"^(?=[A-Z0-9]*[0-9])(?=[A-Z0-9]*[A-Z])[A-Z0-9]{3,10}(?:-(?:[0-9]|1[0-5]))?$")

PASS, FAIL, INCONCLUSIVE = "PASS", "FAIL", "INCONCLUSIVE"
NOT_RUN, MANUAL, NOT_MEASURABLE, INFO = "NOT_RUN", "MANUAL", "NOT_MEASURABLE", "INFO"


@dataclass(frozen=True)
class Target:
    id: str
    row: str
    target: str
    how: str  # the concrete on-air procedure, or why there is none


TARGETS = [
    Target("throughput", "Net payload throughput, one direction, 4 s frames, TXDelay 150 ms",
           ">= 5.0 kbps at 25 dB audio SNR",
           "Session, 1 file each way (A to B, then B to A) at SNR >= 25 dB measured by the receiver; net = payload bits / "
           "time from send to delivered. Judged at the TXDelay configured; adjusted figure for 150 ms reported."),
    Target("min_link", "Minimum usable link", ">= 1.2 kbps net at 8 dB audio SNR (MCS 0)",
           "MCS 0 frames (2 s) each way at 8 dB +/- 2 dB (step attenuator, operator-set); net = Appendix A.2 cycle model x "
           "(1 - block error rate). A.2 gives 1.21 kbps, so any lost block fails the row."),
    Target("adaptation", "Rate adaptation",
           "Correct rung within 2 frames after a 6 dB drop and 4 after a 6 dB rise; no oscillation over 60 s at constant SNR",
           "Session transfer; rung timeline from the API link events. 60 s at constant SNR needs nothing; the 6 dB steps "
           "need a step attenuator (operator prompts, attenuator = manual)."),
    Target("width", "Width negotiation",
           "Usable width settled within one round trip; a masked tone restored within one re-probe",
           "Settled width: one CONNECT, session open, both directions at the negotiated width. Mask restore needs an "
           "impairment injected on a single tone: MANUAL."),
    Target("bler_switch", "Block error rate at each rung's switching point", "<= 10%",
           "Frames at each rung MCS 0-6 at its switching point (threshold + 2 dB, the lowest SNR at which 10.1 steps up), "
           "each direction; BLER over data blocks. Needs the step attenuator."),
    Target("drive_cal", "Transmit drive calibration",
           "Knee within +/-1.5 dB on three radio makes; drive within 10% of target",
           "The modem's `calibrate` command is unsupported (SPEC 13.5, v1.0) and the row needs three makes."),
    Target("distortion", "Distortion report",
           "Index >= 3 within two frames of a 6 dB overdrive, back to 0 within two frames of its removal",
           "Session transfer; operator raises station A's drive by 6 dB (mixer) and restores it; index from B's link events."),
    Target("repeater", "Repeater mode", "10 min through a repeater, net >= 60% of simplex",
           "Needs a repeater with hang time and a courtesy tone, and repeater mode in the modem (a v1.x parameter, "
           "`config` repeater is read-only and off)."),
    Target("resume", "Session resume",
           "60 s outage: no block lost or delivered twice; 120 s outage: resumes, no acknowledged block resent",
           "Session transfer A to B; operator switches radio B off for 60 s, then 120 s (both timed by the script). Evidence: "
           "link_lost or not, resumed flag, delivered, file sha256 at B. Re-sent acknowledged blocks are not visible in the API."),
    Target("link_test", "Link test",
           "Both directions within 3 s of air time and within 20% of the following session",
           "`probe` before the session: keyed time of both stations from the tx events; forward and reverse net kbps "
           "against the A to B and B to A sessions."),
    Target("soft_combining", "Soft combining", "3 repeats decode 4 dB below the single-copy threshold",
           "The receiver does not combine repeats yet (HARQ is planned, SPEC 9.4); nothing to measure."),
    Target("multimode", "Multi-mode receiver", "1200 AFSK and G3RUH captures at least as sensitive as the reference TNC",
           "An offline comparison of captures against the reference TNC, not an on-air test between two stations."),
    Target("fcd", "False carrier-detect rate, squelch open, no signal", "<= 1 per 10 minutes",
           "Both modems idle (listening only) with the squelch open for the run length (default 10 min); carrier-detect "
           "rises from `status`. A decoded frame in the window makes the run INCONCLUSIVE."),
    Target("deference", "Deference to AFSK packet", "No transmission starts within 100 ms of a detected Bell 202 frame",
           "Needs an independent Bell 202 source (`afsk_source_cmd` in the config: a command that sends one packet from a "
           "third station) while A has data queued; dcd and tx events compared. MANUAL when no source is configured."),
    Target("clock", "Sample-clock tolerance", "+/-100 ppm, no block-error penalty",
           "On air the two sound cards' offset is whatever it is: the script reports the observed ppm and BLER (INFO). "
           "The +/-100 ppm span itself is only forced in simulation."),
    Target("interop", "Interoperability", "Two independent implementations, 1 MB each way",
           "There is one implementation; not measurable by this script."),
]
TARGET_BY_ID = {t.id: t for t in TARGETS}
EXTRA_DETECTION = Target("detection", "Preamble detection (SPEC 6.1, not a 14.1 row)", ">= 99% at 6 dB audio SNR",
                         "MCS 0 frames at 6 dB; frames whose header decoded over frames sent. Informational.")

# Simulated stand-ins for the session rows: tests in modem/cli/tests/two_station.rs (two complete modems on the
# simulated FM channel). They check the same behaviours with the reference modem against the simulator.
SIM_PROXIES = {
    "throughput": ["one_way_throughput_against_appendix_a"],
    "adaptation": ["rung_follows_snr_steps"],
    "width": ["masks_follow_a_channel_change"],
    "resume": ["link_lost_for_120s_then_resumed"],
    "link_test": ["link_test_predicts_the_session"],
    "fcd": ["idle_open_squelch_gives_no_false_carrier_detect"],
    "deference": ["bell202_burst_during_a_session_is_deferred_to"],
}
QUICK_PROXIES = ["idle_open_squelch_gives_no_false_carrier_detect"]

NOT_MEASURABLE_ROWS = {"drive_cal", "repeater", "soft_combining", "multimode", "interop"}


class BenchError(Exception):
    """A refusal or a setup failure: printed, exit status 2."""


# --------------------------------------------------------------------------------------
# Configuration
# --------------------------------------------------------------------------------------

@dataclass
class Station:
    name: str  # "a" or "b"
    callsign: str
    device: str = "default"
    ptt: str = "none"
    ptt_line: str = "dtr"
    ptt_invert: bool = False
    rig: str = ""  # a profile of `cairn-modem rigs`
    make: str = ""  # radio make and model, for the milestone count
    ssh: str = ""  # ssh destination (a host or an ssh config alias); empty: this machine
    modem: str = ""  # path of cairn-modem on that machine (default: the repo build, or `cairn-modem` on PATH over ssh)
    grid: str = ""


@dataclass
class Config:
    a: Station
    b: Station
    txdelay_ms: int = 150
    txtail_ms: int = 10
    cap: int = 24
    frequency_notes: str = ""
    path_notes: str = ""  # data port or mic path, attenuation, etc.
    rig_pair: str = ""
    attenuator: str = "none"  # "manual": an operator-set step attenuator, prompts; "none"
    channel_check_s: int = 60
    fcd_s: int = 600
    frames: int = 10
    detection_frames: int = 30
    file_kb: int = 40
    adapt_steady_s: int = 60
    switch_margin_db: float = 2.0
    max_keyed_s: int = 1800
    afsk_source_cmd: str = ""
    api_port_a: int = 18002
    api_port_b: int = 18003
    extra: dict = field(default_factory=dict)


def valid_callsign(cs: str) -> bool:
    return bool(CALLSIGN_RE.match(cs.upper()))


def check_callsigns(cfg: Config, *, on_air: bool) -> None:
    """Refuse missing or malformed callsigns; on air, also placeholders and one shared callsign."""
    for st in (cfg.a, cfg.b):
        cs = st.callsign.strip().upper()
        if on_air and cs.split("-")[0] in FICTITIOUS:
            raise BenchError(f"station {st.name.upper()}: '{cs}' is a placeholder callsign; on-air mode needs your own")
        if not valid_callsign(cs):
            raise BenchError(f"station {st.name.upper()}: '{st.callsign}' is not a callsign (CALL or CALL-SSID, SSID 0-15)")
    if on_air and cfg.a.callsign.upper() == cfg.b.callsign.upper():
        raise BenchError("stations A and B must differ (use different SSIDs of your callsign)")


def load_config(path: str | None, overrides: dict) -> Config:
    raw: dict = {}
    if path:
        with open(path, "rb") as f:
            raw = tomllib.load(f)
    run = dict(raw.get("run", {}))
    run.update({k: v for k, v in overrides.items() if v is not None})
    st = raw.get("station", {})

    def station(name: str, default_call: str) -> Station:
        d = dict(st.get(name, {}))
        d.setdefault("callsign", default_call)
        unknown = set(d) - set(Station.__dataclass_fields__)
        if unknown:
            raise BenchError(f"[station.{name}]: unknown keys {sorted(unknown)}")
        return Station(name=name, **d)

    known = set(Config.__dataclass_fields__) - {"a", "b", "extra"}
    unknown = set(run) - known
    if unknown:
        raise BenchError(f"[run]: unknown keys {sorted(unknown)}")
    cfg = Config(a=station("a", "N0CALL-1"), b=station("b", "N0CALL-2"), **run)
    cfg.a.callsign = cfg.a.callsign.upper()
    cfg.b.callsign = cfg.b.callsign.upper()
    return cfg


# --------------------------------------------------------------------------------------
# Parsing and evaluation (pure functions, unit-testable)
# --------------------------------------------------------------------------------------

FRAME_RE = re.compile(
    r"^\[\s*(?P<t>[\d.]+) s\]\s+(?P<src>\S+) > (?P<dst>\S+)\s+(?P<type>\S+).*?seq\s+(?P<seq>\d+)\s+"
    r"MCS\s+(?P<mcs>\d+)\s+x(?P<width>\d+)\s.*?blocks\s+(?P<ok>\d+)/(?P<n>\d+).*?SNR\s+(?P<snr>[-\d.]+) dB"
    r"(?:\s+clock\s+(?P<ppm>[-+\d.]+) ppm)?")


def parse_listen(text: str) -> dict:
    """Frames decoded by `cairn-modem listen`, header failures and the closing summary line."""
    frames, failures, done = [], 0, ""
    for line in text.splitlines():
        m = FRAME_RE.match(line)
        if m:
            frames.append({"t": float(m["t"]), "src": m["src"], "dst": m["dst"], "type": m["type"], "seq": int(m["seq"]),
                           "mcs": int(m["mcs"]), "width": int(m["width"]), "blocks_ok": int(m["ok"]),
                           "blocks": int(m["n"]), "snr_db": float(m["snr"]),
                           "ppm": float(m["ppm"]) if m["ppm"] else None})
        elif "not decoded" in line:
            failures += 1
        elif line.startswith("done:"):
            done = line
    return {"frames": frames, "header_failures": failures, "done": done}


@dataclass
class FrameRun:
    direction: str
    mcs: int
    sent: int
    decoded: int
    blocks_ok: int
    blocks_sent: int
    snr: list
    ppm: list
    header_failures: int = 0
    target_snr: float | None = None

    @property
    def bler(self) -> float:
        return 1.0 - self.blocks_ok / self.blocks_sent if self.blocks_sent else 1.0

    @property
    def detection(self) -> float:
        return self.decoded / self.sent if self.sent else 0.0

    @property
    def snr_mean(self) -> float | None:
        return statistics.fmean(self.snr) if self.snr else None

    def net_kbps_model(self) -> float:
        """Appendix A.2 net rate at this rung scaled by the blocks that arrived."""
        return A2_NET_KBPS[self.mcs] * (1.0 - self.bler)

    def summary(self) -> dict:
        return {"direction": self.direction, "mcs": self.mcs, "frames_sent": self.sent, "headers_decoded": self.decoded,
                "blocks_ok": self.blocks_ok, "blocks_sent": self.blocks_sent, "bler": round(self.bler, 4),
                "snr_db_mean": None if self.snr_mean is None else round(self.snr_mean, 1),
                "snr_db_range": [round(min(self.snr), 1), round(max(self.snr), 1)] if self.snr else None,
                "ppm_range": [round(min(self.ppm), 1), round(max(self.ppm), 1)] if self.ppm else None,
                "target_snr_db": self.target_snr, "net_kbps_model": round(self.net_kbps_model(), 3),
                "header_failures": self.header_failures}


def frame_run_from_listen(parsed: dict, *, direction: str, mcs: int, sent: int, src: str, target_snr=None) -> FrameRun:
    """Score a listen log against `sent` frames from `src`: a frame that never decoded loses all its blocks."""
    ours = [f for f in parsed["frames"] if f["src"].split("-")[0] == src.split("-")[0] and f["mcs"] == mcs]
    seen = {}
    for f in ours:  # one entry per sequence number (a repeated decode of the same seq is not a second frame)
        seen.setdefault(f["seq"], f)
    ours = list(seen.values())
    per_frame = max([f["blocks"] for f in ours] or [A2_BLOCKS[mcs]])
    return FrameRun(direction, mcs, sent, min(len(ours), sent), sum(f["blocks_ok"] for f in ours), sent * per_frame,
                    [f["snr_db"] for f in ours], [f["ppm"] for f in ours if f["ppm"] is not None],
                    parsed["header_failures"], target_snr)


def rung_for_snr(snr_db: float, margin_db: float = 2.0) -> int:
    """Highest rung whose threshold is at least `margin_db` below the SNR (SPEC 10.1), never below MCS 0."""
    best = 0
    for m, thr in enumerate(THRESHOLD_DB):
        if thr + margin_db <= snr_db:
            best = m
    return best


def count_reversals(seq: list) -> int:
    """Changes of direction in a rung timeline (an up-step then a down-step, or the reverse)."""
    steps = [b - a for a, b in zip(seq, seq[1:], strict=False) if b != a]
    return sum(1 for a, b in zip(steps, steps[1:], strict=False) if (a > 0) != (b > 0))


def frames_to_settle(seq: list) -> tuple[int | None, int | None]:
    """(frames until the rung is the one the timeline ends on and stays there, that final rung); 1-based."""
    if not seq:
        return None, None
    final = statistics.mode(seq[-4:])
    idx = len(seq)
    while idx > 0 and seq[idx - 1] == final:
        idx -= 1
    return idx + 1, final


def frames_until(seq: list, pred) -> int | None:
    for i, v in enumerate(seq, 1):
        if pred(v):
            return i
    return None


def afsk_violations(dcd_events: list, tx_starts: list, guard_s: float = 0.1) -> tuple[int, int]:
    """(Bell 202 bursts seen, transmissions that started inside one or within `guard_s` after it).
    dcd_events: [{"at": s, "afsk": bool}] in time order; tx_starts: modem times of own tx starts."""
    bursts, bad, rise = 0, 0, None
    for ev in dcd_events:
        if ev["afsk"] and rise is None:
            rise = ev["at"]
        elif not ev["afsk"] and rise is not None:
            bursts += 1
            bad += sum(1 for t in tx_starts if rise <= t <= ev["at"] + guard_s)
            rise = None
    return bursts, bad


def row(id_: str, status: str, measured: str, method: str, notes: str = "", evidence=None) -> dict:
    t = TARGET_BY_ID.get(id_, EXTRA_DETECTION)
    return {"id": id_, "row": t.row, "target": t.target, "status": status, "measured": measured, "method": method,
            "notes": notes, "evidence": evidence if evidence is not None else {}}


def eval_min_link(runs: list[FrameRun], tol_db: float = 2.0) -> dict:
    if not runs:
        return row("min_link", NOT_RUN, "", "-", "not run")
    ev = [r.summary() for r in runs]
    snr_ok = all(r.snr_mean is not None and abs(r.snr_mean - 8.0) <= tol_db for r in runs)
    net = min(r.net_kbps_model() for r in runs)
    meas = f"net {net:.2f} kbps (model), BLER {max(r.bler for r in runs) * 100:.1f}%, SNR " + \
        "/".join("-" if r.snr_mean is None else f"{r.snr_mean:.1f}" for r in runs) + " dB"
    if not snr_ok:
        return row("min_link", INCONCLUSIVE, meas, "frames", "SNR outside 8 dB +/- 2 dB (set the attenuator)", ev)
    return row("min_link", PASS if net >= 1.2 else FAIL, meas, "frames",
               "Appendix A.2 cycle model; 1.21 kbps at zero block errors", ev)


def eval_bler_switch(points: list[FrameRun], margin_db: float, tol_db: float = 2.0) -> dict:
    if not points:
        return row("bler_switch", NOT_RUN, "", "-", "not run")
    ev = [p.summary() for p in points]
    bad = [p for p in points if p.bler > 0.10]
    off = [p for p in points if p.snr_mean is None or abs(p.snr_mean - p.target_snr) > tol_db]
    worst = max(points, key=lambda p: p.bler)
    meas = (f"{len(points)} points (7 rungs x each direction), worst BLER {worst.bler * 100:.1f}% at MCS {worst.mcs} "
            f"{worst.direction}; SNR error up to "
            f"{max((abs(p.snr_mean - p.target_snr) for p in points if p.snr_mean is not None), default=float('nan')):.1f} dB")
    if bad:  # a failure is a failure even where another point missed its SNR
        return row("bler_switch", FAIL, meas, "frames",
                   "over 10%: " + ", ".join(f"MCS {p.mcs} {p.direction}" for p in bad), ev)
    if off:
        return row("bler_switch", INCONCLUSIVE, meas, "frames",
                   "SNR off target (+/-2 dB) at: " + ", ".join(f"MCS {p.mcs} {p.direction}" for p in off), ev)
    return row("bler_switch", PASS, meas, "frames", f"switching point = threshold + {margin_db:g} dB", ev)


def eval_clock(runs: list[FrameRun], forced: bool) -> dict:
    ev = [r.summary() for r in runs]
    if not forced:
        ppms = [p for r in runs for p in r.ppm]
        if not ppms:
            return row("clock", NOT_RUN, "", "-", "no clock readings", ev)
        return row("clock", INFO, f"observed {min(ppms):+.0f}..{max(ppms):+.0f} ppm, BLER "
                   f"{max(r.bler for r in runs) * 100:.1f}%", "frames",
                   "the +/-100 ppm span cannot be forced on air; this only shows the pair's own offset", ev)
    base = next(r for r in runs if r.target_snr is not None and r.ppm_forced == 0)  # type: ignore[attr-defined]
    worst = max(r.bler - base.bler for r in runs)
    return row("clock", PASS if worst <= 0.02 else FAIL,
               f"BLER penalty at +/-100 ppm {worst * 100:+.1f} points (baseline {base.bler * 100:.1f}%)", "sim-frames",
               "", ev)


def binom_tail(n: int, k: int, p: float) -> float:
    """P(at least k successes in n trials of probability p)."""
    from math import comb
    return sum(comb(n, i) * p**i * (1 - p) ** (n - i) for i in range(k, n + 1))


def eval_detection(r: FrameRun | None) -> dict:
    """99% detection is only demonstrable with about 100 frames: a miss rate the target allows (1%) must be
    unlikely (under 5%) before this is called a FAIL, and PASS needs 100 frames."""
    if r is None:
        return row("detection", NOT_RUN, "", "-", "not run")
    miss = r.sent - r.decoded
    snr = "-" if r.snr_mean is None else f"{r.snr_mean:.1f}"
    meas = f"{r.decoded}/{r.sent} headers decoded at {snr} dB"
    if miss and binom_tail(r.sent, miss, 0.01) < 0.05:
        return row("detection", FAIL, meas, "frames", f"{miss} missed: a 1% miss rate would do this less than 5% of the time",
                   [r.summary()])
    if r.sent >= 100 and r.detection >= 0.99:
        return row("detection", PASS, meas, "frames", f"n = {r.sent}", [r.summary()])
    return row("detection", INFO, meas, "frames", f"n = {r.sent}: under 100 frames cannot show 99%; not a failure"
               if miss else f"n = {r.sent}: under 100 frames cannot show 99%", [r.summary()])


# --------------------------------------------------------------------------------------
# Running commands: local, or over ssh
# --------------------------------------------------------------------------------------

SSH_OPTS = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=15"]


def modem_flags(st: Station, cfg: Config, *, ptt: bool) -> list[str]:
    """Flags common to the commands of one station. `ptt` False for listen-only commands (they never key)."""
    f = ["--no-config", "-d", st.device]
    if st.rig:
        f += ["--rig", st.rig]
    if ptt:
        f += ["--ptt", st.ptt, "--ptt-line", st.ptt_line, "--txdelay", str(cfg.txdelay_ms), "--txtail", str(cfg.txtail_ms),
              "--callsign", st.callsign]
        if st.ptt_invert:
            f.append("--ptt-invert")
    return f


def wrap(st: Station, argv: list[str]) -> list[str]:
    return ["ssh", *SSH_OPTS, st.ssh, "--", shlex.join(argv)] if st.ssh else argv


def run_cmd(st: Station, argv: list[str], timeout: float, log: Path | None = None) -> subprocess.CompletedProcess:
    cp = subprocess.run(wrap(st, argv), capture_output=True, text=True, timeout=timeout, stdin=subprocess.DEVNULL)
    if log:
        log.write_text(f"$ {shlex.join(wrap(st, argv))}\n--- stdout\n{cp.stdout}\n--- stderr\n{cp.stderr}\n--- exit {cp.returncode}\n")
    return cp


def popen(st: Station, argv: list[str]) -> subprocess.Popen:
    return subprocess.Popen(wrap(st, argv), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                            stdin=subprocess.DEVNULL, start_new_session=True)


def leftover_modems(st: Station) -> str:
    """Running cairn-modem processes on the station's machine. `-x` matches the process name, never a command line,
    so pgrep cannot match itself, the ssh command or this script (whose arguments may name the binary)."""
    cp = run_cmd(st, ["pgrep", "-a", "-x", "cairn-modem"], timeout=30)
    return cp.stdout.strip()


# --------------------------------------------------------------------------------------
# The native API client (docs/api.md): JSON lines over TCP
# --------------------------------------------------------------------------------------

class ApiConn:
    def __init__(self, port: int, timeout: float = 15.0) -> None:
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.sock.settimeout(None)
        self.events: list[dict] = []
        self.responses: dict = {}
        self.cv = threading.Condition()
        self.next_id = 1
        self.closed = False
        self.t0 = time.monotonic()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self) -> None:
        try:
            for line in self.sock.makefile("r", encoding="utf-8", newline="\n"):
                try:
                    o = json.loads(line)
                except ValueError:
                    continue
                o["_t"] = time.monotonic() - self.t0
                with self.cv:
                    if "event" in o:
                        self.events.append(o)
                    elif "id" in o:
                        self.responses[o["id"]] = o
                    self.cv.notify_all()
        except (OSError, ValueError):
            pass
        self.closed = True
        with self.cv:
            self.cv.notify_all()

    def request(self, cmd: str, timeout: float = 20.0, **kw) -> dict:
        with self.cv:
            rid = self.next_id
            self.next_id += 1
        self.sock.sendall((json.dumps({"id": rid, "cmd": cmd, **kw}) + "\n").encode())
        end = time.monotonic() + timeout
        with self.cv:
            while rid not in self.responses:
                left = end - time.monotonic()
                if left <= 0 or self.closed:
                    raise BenchError(f"API: no response to {cmd}")
                self.cv.wait(left)
            r = self.responses.pop(rid)
        if not r.get("ok"):
            raise BenchError(f"API {cmd}: {r.get('error') or r}")
        return r

    def now(self) -> float:
        return time.monotonic() - self.t0

    def mark(self) -> int:
        with self.cv:
            return len(self.events)

    def wait(self, pred, timeout: float, since: int = 0) -> dict | None:
        end = time.monotonic() + timeout
        i = since
        with self.cv:
            while True:
                while i < len(self.events):
                    if pred(self.events[i]):
                        return self.events[i]
                    i += 1
                left = end - time.monotonic()
                if left <= 0 or self.closed:
                    return None
                self.cv.wait(min(left, 1.0))

    def select(self, pred, since: int = 0, until: float | None = None) -> list[dict]:
        with self.cv:
            return [e for e in self.events[since:] if pred(e) and (until is None or e["_t"] <= until)]

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


def is_state(state: str):
    return lambda e: e.get("event") == "session" and e.get("kind") == "state" and e.get("state") == state


def link_frames(conn: ApiConn, since: int, until: float | None = None) -> list[dict]:
    return conn.select(lambda e: e.get("event") == "link" and e.get("kind") == "frame", since, until)


# --------------------------------------------------------------------------------------
# Operator prompts
# --------------------------------------------------------------------------------------

class Operator:
    def __init__(self, interactive: bool) -> None:
        self.interactive = interactive

    def ask(self, msg: str) -> str:
        if not self.interactive:
            raise BenchError(f"operator action needed but --non-interactive: {msg}")
        print(f"\n>>> {msg}", flush=True)
        return input("    press Enter when done (or type 'skip'): ").strip().lower()


# --------------------------------------------------------------------------------------
# Labs: where the frames and sessions come from
# --------------------------------------------------------------------------------------

class Lab:
    """Interface of the two back ends. frame_run is the one-way measurement both use; sessions differ."""
    on_air = False
    keyed_s: dict

    def preflight(self) -> dict: ...
    def channel_check(self) -> dict: ...
    def frame_run(self, direction: str, mcs: int, count: int, snr: float | None = None, *, ppm: float = 0.0,
                  gain_db: float = 0.0, seed: int = 1) -> FrameRun | None: ...
    def reach_snr(self, direction: str, target: float) -> None: ...
    def session_rows(self, wanted: list[str]) -> list[dict]: ...
    def close(self) -> None: ...


class SimLab(Lab):
    """tx --wav, the Stage 1 FM channel (modem/tools/fm_channel_wav.py --serve), listen --wav; cargo test for sessions."""

    def __init__(self, cfg: Config, work: Path, modem: Path, profile: str, proxies: str) -> None:
        self.cfg, self.work, self.modem, self.profile, self.proxies = cfg, work, modem, profile, proxies
        self.keyed_s = {"a": 0.0, "b": 0.0}
        self.n = 0
        tool = REPO / "modem" / "tools" / "fm_channel_wav.py"
        cmd = ["uv", "run", "--package", "cairn-sim", "python", str(tool), "--serve"] if shutil.which("uv") \
            else [sys.executable, str(tool), "--serve"]
        self.chan = subprocess.Popen(cmd, cwd=REPO, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)

    def close(self) -> None:
        if self.chan.poll() is None:
            self.chan.stdin.close()
            try:
                self.chan.wait(10)
            except subprocess.TimeoutExpired:
                self.chan.kill()

    def preflight(self) -> dict:
        v = subprocess.run([str(self.modem), "--version"], capture_output=True, text=True).stdout.strip()
        return {"mode": "simulation", "cairn_modem": v}

    def channel_check(self) -> dict:
        # open-squelch noise only, as the on-air check hears an empty channel
        import array
        import random
        import wave
        wav = self.work / "chan_check.wav"
        rng = random.Random(7)
        pcm = array.array("h", (int(max(-1.0, min(1.0, rng.gauss(0, 0.03))) * 32767) for _ in range(48000 * 6)))
        with wave.open(str(wav), "wb") as w:
            w.setnchannels(1)
            w.setsampwidth(2)
            w.setframerate(48000)
            w.writeframes(pcm.tobytes())
        cp = subprocess.run([str(self.modem), "--no-config", "listen", "--wav", str(wav), "--stats", "0"],
                            capture_output=True, text=True)
        p = parse_listen(cp.stdout)
        return {"busy": bool(p["frames"]) or p["header_failures"] > 0, "frames_heard": len(p["frames"]),
                "detail": p["done"]}

    def _channel(self, src: Path, dst: Path, *, snr, ppm, gain_db, seed) -> None:
        job = {"inp": str(src), "out": str(dst), "profile": self.profile, "snr": snr, "ppm": ppm, "gain_db": gain_db,
               "seed": seed}
        self.chan.stdin.write(json.dumps(job) + "\n")
        self.chan.stdin.flush()
        reply = json.loads(self.chan.stdout.readline() or "{}")
        if not reply.get("ok"):
            raise BenchError(f"simulated channel failed: {reply}")

    def frame_run(self, direction, mcs, count, snr=None, *, ppm=0.0, gain_db=0.0, seed=1):
        self.n += 1
        src, dst = (self.cfg.a, self.cfg.b) if direction == "a2b" else (self.cfg.b, self.cfg.a)
        tag = f"{self.n:03d}_{direction}_m{mcs}"
        tx = self.work / f"{tag}.wav"
        rx = self.work / f"{tag}_ch.wav"
        cp = subprocess.run([str(self.modem), "--no-config", "tx", "--wav", str(tx), "--callsign", src.callsign, "--id",
                             "--mcs", str(mcs), "--count", str(count), "--gap", "0.7", "--seed", str(seed)],
                            capture_output=True, text=True)
        if cp.returncode:
            raise BenchError(f"cairn-modem tx --wav failed: {cp.stderr or cp.stdout}")
        self._channel(tx, rx, snr=25.0 if snr is None else snr, ppm=ppm, gain_db=gain_db, seed=seed + self.n)
        cp = subprocess.run([str(self.modem), "--no-config", "listen", "--wav", str(rx), "--stats", "0"],
                            capture_output=True, text=True)
        (self.work / f"{tag}.log").write_text(cp.stdout)
        self.keyed_s[src.name] += count * FRAME_AIR_S[mcs]
        fr = frame_run_from_listen(parse_listen(cp.stdout), direction=direction, mcs=mcs, sent=count, src=src.callsign,
                                   target_snr=snr)
        fr.ppm_forced = ppm  # type: ignore[attr-defined]
        return fr

    def reach_snr(self, direction, target) -> None:
        pass  # the simulated channel is set to the target directly

    def session_rows(self, wanted):
        rows: list[dict] = []
        names = {t for r in wanted for t in SIM_PROXIES.get(r, [])}
        if self.proxies == "none":
            names = set()
        elif self.proxies == "quick":
            names &= set(QUICK_PROXIES)
        by_test = {}
        for n in sorted(names):
            t0 = time.monotonic()
            cp = subprocess.run(["cargo", "test", "--release", "--manifest-path", str(REPO / "modem" / "Cargo.toml"),
                                 "-p", "cairn-modem", "--test", "two_station", n, "--", "--exact"],
                                capture_output=True, text=True, cwd=REPO)
            by_test[n] = (cp.returncode == 0 and "1 passed" in cp.stdout, round(time.monotonic() - t0, 1),
                          cp.stdout[-400:] if cp.returncode else "")
        for r in wanted:
            tests = SIM_PROXIES.get(r)
            if not tests:
                rows.append(row(r, NOT_RUN, "", "-", "no simulated stand-in; on air only"))
            elif not all(t in by_test for t in tests):
                rows.append(row(r, NOT_RUN, "", "-", "simulated stand-in not selected (--sim-proxies)"))
            else:
                ok = all(by_test[t][0] for t in tests)
                rows.append(row(r, PASS if ok else FAIL, "two simulated modems: " + ", ".join(
                    f"{t} {'ok' if by_test[t][0] else 'FAILED'} ({by_test[t][1]} s)" for t in tests), "sim-proxy",
                    "cargo test, modem/cli/tests/two_station.rs; asserts this row's target in the simulator",
                    {t: {"passed": by_test[t][0], "seconds": by_test[t][1], "tail": by_test[t][2]} for t in tests}))
        if "distortion" in wanted:
            rows.append(row("distortion", NOT_RUN, "", "-",
                            "the modem's distortion measurement is a constant 0 (runtime.rs); nothing to simulate"))
        return rows


class AirLab(Lab):
    on_air = True

    def __init__(self, cfg: Config, work: Path, op: Operator, modem_a: str, modem_b: str) -> None:
        self.cfg, self.work, self.op = cfg, work, op
        self.keyed_s = {"a": 0.0, "b": 0.0}
        self.n = 0
        self._snr_set: float | None = None
        self.modems = {"a": modem_a, "b": modem_b}

    def close(self) -> None:
        pass

    def _argv(self, st: Station, *rest: str) -> list[str]:
        return [self.modems[st.name], *rest]

    def preflight(self) -> dict:
        info: dict = {"mode": "on-air"}
        for st in (self.cfg.a, self.cfg.b):
            left = leftover_modems(st)
            if left:
                raise BenchError(f"station {st.name.upper()}: cairn-modem already running (it holds the audio device and "
                                 f"may hold PTT); stop it first:\n{left}")
            v = run_cmd(st, self._argv(st, "--version"), 30)
            info[f"cairn_modem_{st.name}"] = v.stdout.strip() or v.stderr.strip()
            # selftest checks devices, PTT spec and the receive level without keying (no --transmit)
            cp = run_cmd(st, self._argv(st, "--no-config", "selftest", "-d", st.device, "--ptt", st.ptt,
                                        "--callsign", st.callsign, *(["--rig", st.rig] if st.rig else [])), 120,
                         self.work / f"selftest_{st.name}.log")
            info[f"selftest_{st.name}"] = {"exit": cp.returncode, "tail": cp.stdout.strip().splitlines()[-6:]}
            if cp.returncode:
                raise BenchError(f"station {st.name.upper()}: selftest failed (see selftest_{st.name}.log)")
        return info

    def channel_check(self) -> dict:
        """Both stations listen only (no PTT flags: these commands cannot key) for channel_check_s."""
        secs = self.cfg.channel_check_s
        procs = {}
        for st in (self.cfg.a, self.cfg.b):
            procs[st.name] = popen(st, self._argv(st, "--no-config", "listen", "-d", st.device, "--seconds", str(secs),
                                                  "--stats", "0"))
        out = {}
        for name, p in procs.items():
            text, _ = p.communicate(timeout=secs + 60)
            (self.work / f"channel_check_{name}.log").write_text(text)
            out[name] = parse_listen(text)
        frames = sum(len(v["frames"]) for v in out.values())
        fails = sum(v["header_failures"] for v in out.values())
        det = [re.search(r"detections (\d+)", v["done"]) for v in out.values()]
        detections = sum(int(m[1]) for m in det if m)
        return {"busy": bool(frames or fails or detections), "frames_heard": frames, "header_failures": fails,
                "detections": detections, "seconds": secs}

    def reach_snr(self, direction: str, target: float) -> None:
        if self.cfg.attenuator != "manual" or self._snr_set == target:
            return  # the path is reciprocal enough: the second direction at the same level is only checked afterwards
        self._snr_set = target
        for _ in range(6):
            probe = self.frame_run(direction, 0, 3, None)
            s = probe.snr_mean if probe else None
            if s is not None and abs(s - target) <= 2.0:
                return
            if s is None:
                ans = self.op.ask(f"{direction}: no frame decoded. Reduce the attenuation until frames decode "
                                  f"(target {target:g} dB audio SNR)")
            else:
                d = s - target
                ans = self.op.ask(f"{direction}: measured {s:.1f} dB, want {target:g} dB: "
                                  f"{'ADD' if d > 0 else 'REMOVE'} {abs(d):.1f} dB of attenuation")
            if ans == "skip":
                return

    def frame_run(self, direction, mcs, count, snr=None, *, ppm=0.0, gain_db=0.0, seed=1):
        cfg = self.cfg
        src, dst = (cfg.a, cfg.b) if direction == "a2b" else (cfg.b, cfg.a)
        self.n += 1
        tag = f"{self.n:03d}_{direction}_m{mcs}"
        air = count * (FRAME_AIR_S[mcs] + 2.0 + (cfg.txdelay_ms - 150) / 1000)
        if self.keyed_s[src.name] + air > cfg.max_keyed_s:
            raise BenchError(f"station {src.name.upper()}: keyed-time budget of {cfg.max_keyed_s} s would be exceeded")
        listener = popen(dst, self._argv(dst, "--no-config", "listen", "-d", dst.device, "--seconds", str(int(air + 12)),
                                         "--stats", "0", *(["--rig", dst.rig] if dst.rig else [])))
        time.sleep(2.5)  # let the listener open the device before anything is keyed
        cp = run_cmd(src, self._argv(src, *modem_flags(src, cfg, ptt=True), "tx", "--id", "--mcs", str(mcs), "--count",
                                     str(count), "--gap", "2", "--seed", str(seed),
                                     *(["--grid", src.grid] if src.grid else [])), air + 60,
                     self.work / f"{tag}_tx.log")
        text, _ = listener.communicate(timeout=air + 60)
        (self.work / f"{tag}.log").write_text(text)
        self.keyed_s[src.name] += air
        if cp.returncode:
            raise BenchError(f"station {src.name.upper()}: tx failed (exit {cp.returncode}); see {tag}_tx.log")
        return frame_run_from_listen(parse_listen(text), direction=direction, mcs=mcs, sent=count, src=src.callsign,
                                     target_snr=snr)

    # ---- sessions: both stations run `cairn-modem run`, driven through the API ----

    def session_rows(self, wanted: list[str]) -> list[dict]:
        return AirSessions(self).run(wanted)


class AirSessions:
    def __init__(self, lab: AirLab) -> None:
        self.lab, self.cfg, self.op, self.work = lab, lab.cfg, lab.op, lab.work
        self.procs: list[tuple[Station, str]] = []  # (station, remote pid file) for remote; local procs below
        self.local: dict[str, subprocess.Popen] = {}
        self.tunnel: subprocess.Popen | None = None
        self.conn: dict[str, ApiConn] = {}

    # -- process management --
    def start(self) -> None:
        cfg = self.cfg
        for st, port in ((cfg.a, cfg.api_port_a), (cfg.b, cfg.api_port_b)):
            argv = self.lab._argv(st, "--no-config", "run", "-d", st.device,
                                  "--ptt", st.ptt, "--ptt-line", st.ptt_line, "--txdelay", str(cfg.txdelay_ms),
                                  "--txtail", str(cfg.txtail_ms), "--callsign", st.callsign, "--cap", str(cfg.cap),
                                  "--api", f"127.0.0.1:{port}", "--kiss", "off", "--no-stdin", "--rx-dir",
                                  str(self._rxdir(st)), "--tx-budget", str(cfg.max_keyed_s), "-v",
                                  *(["--ptt-invert"] if st.ptt_invert else []), *(["--rig", st.rig] if st.rig else []),
                                  *(["--grid", st.grid] if st.grid else []))
            log = self.work / f"run_{st.name}.log"
            if st.ssh:
                # setsid nohup with stdin from /dev/null so the modem outlives the ssh command; its pid goes to a file
                inner = f"echo $$ > {shlex.quote(str(self._pidfile(st)))}; exec {shlex.join(argv)}"
                sh = f"mkdir -p {shlex.quote(str(self._rxdir(st)))}; (setsid nohup sh -c {shlex.quote(inner)} " \
                     f"</dev/null >{shlex.quote(str(self._remote_log(st)))} 2>&1 &)"
                cp = run_cmd(st, ["sh", "-c", sh], 60)
                if cp.returncode:
                    raise BenchError(f"station {st.name.upper()}: could not start the modem over ssh: {cp.stderr}")
            else:
                self._rxdir(st).mkdir(parents=True, exist_ok=True)
                self.local[st.name] = subprocess.Popen(argv, stdout=open(log, "w"), stderr=subprocess.STDOUT,
                                                       stdin=subprocess.DEVNULL, start_new_session=True)
        time.sleep(6)
        if cfg.b.ssh:
            self.tunnel = subprocess.Popen(["ssh", *SSH_OPTS, "-N", "-L", f"{cfg.api_port_b}:127.0.0.1:{cfg.api_port_b}",
                                            cfg.b.ssh], stdin=subprocess.DEVNULL)
            time.sleep(2)
        for st, port in ((cfg.a, cfg.api_port_a), (cfg.b, cfg.api_port_b)):
            self.conn[st.name] = ApiConn(port)
            hello = self.conn[st.name].wait(lambda e: e.get("event") == "hello", 10)
            if not hello or hello.get("station", "").upper() != st.callsign:
                raise BenchError(f"station {st.name.upper()}: API hello says {hello and hello.get('station')}, "
                                 f"expected {st.callsign}")

    def _rxdir(self, st: Station) -> Path:
        return (Path("/tmp") / "cairn-bench-14-1" / st.name / "rx") if st.ssh else self.work / f"rx_{st.name}"

    def _pidfile(self, st: Station) -> Path:
        return Path("/tmp") / "cairn-bench-14-1" / f"{st.name}.pid"

    def _remote_log(self, st: Station) -> Path:
        return Path("/tmp") / "cairn-bench-14-1" / f"run_{st.name}.log"

    def stop(self) -> None:
        for c in self.conn.values():
            c.close()
        for st in (self.cfg.a, self.cfg.b):
            if st.ssh:
                # kill by pid file (SIGTERM releases PTT), then fetch the log; never pkill -f from a shell whose own
                # command line would match
                run_cmd(st, ["sh", "-c", f"test -f {shlex.quote(str(self._pidfile(st)))} && "
                             f"kill $(cat {shlex.quote(str(self._pidfile(st)))}); sleep 1; true"], 30)
                cp = run_cmd(st, ["cat", str(self._remote_log(st))], 60)
                (self.work / f"run_{st.name}.log").write_text(cp.stdout)
            elif st.name in self.local:
                p = self.local[st.name]
                p.terminate()
                try:
                    p.wait(15)
                except subprocess.TimeoutExpired:
                    p.kill()
        if self.tunnel:
            self.tunnel.terminate()

    # -- helpers --
    def status(self, who: str) -> dict:
        return self.conn[who].request("status")

    def gen_file(self, who: str, kb: int, name: str) -> tuple[str, str]:
        """A random file on station `who`'s machine; returns its path and sha256."""
        st = self.cfg.a if who == "a" else self.cfg.b
        path = f"/tmp/cairn-bench-14-1/{who}_{name}.bin"
        cp = run_cmd(st, ["sh", "-c", f"mkdir -p /tmp/cairn-bench-14-1 && head -c {kb * 1024} /dev/urandom > {path} "
                          f"&& sha256sum {path}"], 60)
        if cp.returncode:
            raise BenchError(f"could not make a test file on station {who.upper()}: {cp.stderr}")
        return path, cp.stdout.split()[0]

    def transfer(self, src: str, dst: str, kb: int, name: str, timeout: float) -> dict:
        """Send one file src to dst in the open session; elapsed seconds, delivered, sha256 match."""
        c_s, c_d = self.conn[src], self.conn[dst]
        path, sha = self.gen_file(src, kb, name)
        ms, md = c_s.mark(), c_d.mark()
        t0 = c_s.now()
        c_s.request("send", file=path)
        ev = c_s.wait(lambda e: e.get("event") == "session" and e.get("kind") in ("delivered", "undelivered"), timeout, ms)
        t1 = c_s.now() if ev is None else ev["_t"]
        saved = c_d.wait(lambda e: e.get("event") == "rx" and (e.get("saved") or {}).get("complete"), 20, md)
        got = (saved or {}).get("saved", {}).get("sha256")
        tx_frames_s = [e for e in c_s.select(lambda e: e.get("event") == "tx" and e.get("kind") == "start", ms, t1)]
        rx_snr = [e["snr_db"] for e in link_frames(c_d, md, t1) if e.get("frame_type") == "DATA"]
        elapsed = t1 - t0
        res = {"bytes": kb * 1024, "elapsed_s": round(elapsed, 2), "delivered": bool(ev and ev["kind"] == "delivered"),
               "sha256_match": got == sha, "net_kbps": round(kb * 1024 * 8 / elapsed / 1000, 3) if elapsed else None,
               "snr_db_median": round(statistics.median(rx_snr), 1) if rx_snr else None, "tx_frames": len(tx_frames_s),
               "rungs": [e.get("mcs") for e in tx_frames_s]}
        return res

    # -- the measurements --
    def run(self, wanted: list[str]) -> list[dict]:
        rows: list[dict] = []
        try:
            self.start()
            rows += self._all(wanted)
        finally:
            self.stop()
        return rows

    def _all(self, wanted: list[str]) -> list[dict]:
        cfg, rows = self.cfg, []
        a = self.conn["a"]
        if "fcd" in wanted:
            rows.append(self._fcd())
        if "link_test" in wanted or "throughput" in wanted or "width" in wanted:
            lt = self._probe()
        else:
            lt = None
        m = a.mark()
        t_conn = a.now()
        a.request("connect", peer=cfg.b.callsign, resume=False)
        opened = a.wait(is_state("open"), 60, m)
        if not opened:
            return rows + [row(r, INCONCLUSIVE, "", "air", "the session did not open") for r in wanted
                           if r in ("throughput", "link_test", "width", "adaptation", "resume", "distortion")]
        connects = [e for e in a.select(lambda e: e.get("event") == "tx" and e.get("kind") == "start", m)
                    if e.get("frame_type") == "CONNECT"]
        stA, stB = self.status("a")["session"], self.status("b")["session"]
        if "width" in wanted:
            ok = len(connects) == 1 and stA["tx"]["width"] == stA["rx"]["width"] == stB["tx"]["width"] == stB["rx"]["width"]
            rows.append(row("width", PASS if ok else FAIL,
                            f"{len(connects)} CONNECT, open after {opened['_t'] - t_conn:.1f} s, widths A tx/rx "
                            f"{stA['tx']['width']}/{stA['rx']['width']}, B {stB['tx']['width']}/{stB['rx']['width']}", "air",
                            "mask restore after an impairment needs one tone knocked out and restored: MANUAL",
                            {"status_a": stA, "status_b": stB, "mask_restore": MANUAL}))
        res = {}
        if "throughput" in wanted or "link_test" in wanted:
            res["a2b"] = self.transfer("a", "b", cfg.file_kb, "tp_a2b", 600)
            res["b2a"] = self.transfer("b", "a", cfg.file_kb, "tp_b2a", 600)
        if "throughput" in wanted:
            rows.append(self._eval_throughput(res))
        if "link_test" in wanted:
            rows.append(self._eval_link_test(lt, res))
        if "adaptation" in wanted:
            rows.append(self._adaptation())
        if "distortion" in wanted:
            rows.append(self._distortion())
        if "resume" in wanted:
            rows.append(self._resume())
        if "deference" in wanted:
            rows.append(self._deference())
        return rows

    def _fcd(self) -> dict:
        cfg = self.cfg
        before = {w: self.status(w) for w in "ab"}
        print(f"idle listen for {cfg.fcd_s} s on both stations (no transmission) ...", flush=True)
        time.sleep(cfg.fcd_s)
        after = {w: self.status(w) for w in "ab"}
        rises, frames = {}, 0
        for w in "ab":
            rb, ra = before[w]["counters"]["dcd_rises"], after[w]["counters"]["dcd_rises"]
            rises[w] = sum(ra.values()) - sum(rb.values())
            frames += after[w]["counters"]["frames"] - before[w]["counters"]["frames"]
        ev = {"seconds": cfg.fcd_s, "rises": rises, "frames_decoded": frames,
              "floor_dbfs": {w: after[w]["dcd"].get("quieting_floor_dbfs") for w in "ab"}}
        scale = 600.0 / cfg.fcd_s
        meas = f"rises A {rises['a']}, B {rises['b']} in {cfg.fcd_s / 60:.0f} min"
        if frames:
            return row("fcd", INCONCLUSIVE, meas, "air", f"{frames} frame(s) decoded: the channel was not empty", ev)
        if any(v is None for v in ev["floor_dbfs"].values()):
            return row("fcd", INCONCLUSIVE, meas, "air", "quieting floor never armed (squelch not open?)", ev)
        worst = max(rises.values()) * scale
        return row("fcd", PASS if worst <= 1.0 else FAIL, meas, "air",
                   "" if cfg.fcd_s >= 600 else f"shorter than 10 min: scaled x{scale:.1f}", ev)

    def _probe(self) -> dict:
        a, b = self.conn["a"], self.conn["b"]
        ma, mb = a.mark(), b.mark()
        a.request("probe", target=self.cfg.b.callsign)
        ev = a.wait(lambda e: e.get("event") == "link" and e.get("kind") == "probe", 30, ma)
        keyed = sum(e.get("keyed_s", 0) for c, m in ((a, ma), (b, mb)) for e in
                    c.select(lambda e: e.get("event") == "tx" and e.get("kind") == "start", m))
        return {"event": ev, "keyed_s": round(keyed, 2)}

    def _eval_throughput(self, res: dict) -> dict:
        cfg = self.cfg
        parts, status = [], PASS
        ev = {}
        for d in ("a2b", "b2a"):
            r = res[d]
            frames = r["tx_frames"]
            # remove the TXDelay in excess of 150 ms from every keyed transmission of both stations (data + ACK)
            adj_s = max(r["elapsed_s"] - 2 * frames * max(cfg.txdelay_ms - 150, 0) / 1000, 1e-9)
            r["net_kbps_at_txdelay_150"] = round(r["bytes"] * 8 / adj_s / 1000, 3)
            ev[d] = r
            parts.append(f"{d[0].upper()}>{d[2].upper()} {r['net_kbps']} kbps @ {r['snr_db_median']} dB")
            if not r["delivered"] or not r["sha256_match"]:
                status = FAIL
            elif r["snr_db_median"] is None or r["snr_db_median"] < 24.0:
                status = INCONCLUSIVE if status == PASS else status
            elif r["net_kbps"] < 5.0:
                status = FAIL
        notes = f"TXDelay {cfg.txdelay_ms} ms (target defined at 150); at 150 ms: " + \
            ", ".join(f"{d} {ev[d]['net_kbps_at_txdelay_150']} kbps" for d in ev)
        if status == INCONCLUSIVE:
            notes += "; SNR below 24 dB: lower the attenuation / raise the signal and rerun"
        return row("throughput", status, "; ".join(parts), "air", notes, ev)

    def _eval_link_test(self, lt: dict | None, res: dict) -> dict:
        ev = lt["event"] if lt else None
        if not ev or not ev.get("answered"):
            return row("link_test", FAIL, "probe unanswered", "air", "", {"probe": ev})
        fw, rv = ev["forward"]["net_kbps"], ev["reverse"]["net_kbps"]
        sa, sb = res["a2b"]["net_kbps"], res["b2a"]["net_kbps"]
        dfw, drv = abs(fw - sa) / sa, abs(rv - sb) / sb
        ok = lt["keyed_s"] <= 3.0 and dfw <= 0.20 and drv <= 0.20
        return row("link_test", PASS if ok else FAIL,
                   f"{lt['keyed_s']} s keyed; forward {fw} vs {sa} kbps ({dfw * 100:.0f}%), reverse {rv} vs {sb} "
                   f"({drv * 100:.0f}%)", "air", "air time = keyed seconds of both stations' tx events during the probe",
                   {"probe": ev, "keyed_s": lt["keyed_s"], "sessions": {k: v["net_kbps"] for k, v in res.items()}})

    def _last_rung(self, c: ApiConn) -> int | None:
        with c.cv:
            for e in reversed(c.events):
                if e.get("event") == "link" and e.get("kind") == "frame" and e.get("tx"):
                    return e["tx"]["mcs"]
        return None

    def _rungs(self, c: ApiConn, since: int, until: float | None = None) -> list[int]:
        return [e["tx"]["mcs"] for e in link_frames(c, since, until) if e.get("tx")]

    def _adaptation(self) -> dict:
        cfg, a = self.cfg, self.conn["a"]
        ev: dict = {}
        path, _ = self.gen_file("a", 400, "adapt")  # long enough that the queue never empties during a phase
        a.request("send", file=path)
        time.sleep(10)  # let the first rungs settle before the constant-SNR window
        w0 = a.mark()
        time.sleep(cfg.adapt_steady_s)
        steady = self._rungs(a, w0)
        ev["steady_rungs"] = steady
        rev = count_reversals(steady)
        parts = [f"60 s steady: {len(steady)} frames, {rev} reversals, rungs {sorted(set(steady))}"]
        status = PASS if rev == 0 and steady else (FAIL if steady else INCONCLUSIVE)
        notes = []
        if cfg.attenuator != "manual":
            notes.append("6 dB steps not run (attenuator = none)")
            res = row("adaptation", INCONCLUSIVE if status == PASS else status, "; ".join(parts), "air",
                      "; ".join(notes), ev)
            a.request("disconnect", graceful=False)
            time.sleep(3)
            return res
        for name, instr, limit in (("drop", "ADD 6 dB of attenuation", 2), ("rise", "REMOVE that 6 dB of attenuation", 4)):
            before = [self._last_rung(a)]
            self.op.ask(f"{instr} now (between the radios, during the transfer); press Enter the moment it is done")
            mk = a.mark()  # frames are counted from the Enter
            time.sleep(40)
            seq = self._rungs(a, mk)
            n, final = frames_to_settle(seq)
            ev[name] = {"rung_before": before[0], "rungs": seq, "settled_in": n, "final": final}
            if n is None or final == before[0]:
                parts.append(f"{name}: rung did not change ({before[0]} -> {final})")
                status = INCONCLUSIVE if status == PASS else status
            else:
                parts.append(f"{name}: MCS {before[0]} -> {final} in {n} frames (limit {limit})")
                if n > limit:
                    status = FAIL
        a.request("disconnect", graceful=False)
        return row("adaptation", status, "; ".join(parts), "air",
                   "frame 1 = first frame A decodes from B after the step; " + "; ".join(notes), ev)

    def _distortion(self) -> dict:
        b, a = self.conn["b"], self.conn["a"]
        if self.status("a")["session"] is None:
            a.request("connect", peer=self.cfg.b.callsign)
            a.wait(is_state("open"), 60, a.mark())
        path, _ = self.gen_file("a", 400, "dist")
        mk0 = b.mark()
        a.request("send", file=path)
        time.sleep(15)
        dist = lambda m: [e.get("distortion", 0) for e in link_frames(b, m) if e.get("frame_type") == "DATA"]  # noqa: E731
        base = dist(mk0)
        self.op.ask("RAISE station A's transmit drive by 6 dB (its playback level in the mixer), note the setting, and "
                    "press Enter the moment it is done")
        mk = b.mark()
        time.sleep(25)
        over = dist(mk)
        rise = frames_until(over, lambda v: v >= 3)
        self.op.ask("RESTORE station A's transmit drive to the level you set (Enter when done)")
        mk2 = b.mark()
        time.sleep(25)
        back = dist(mk2)
        clear = frames_until(back, lambda v: v == 0)
        a.request("disconnect", graceful=False)
        time.sleep(3)
        ev = {"baseline": base, "overdriven": over, "restored": back}
        ok = rise is not None and rise <= 2 and clear is not None and clear <= 2
        note = "index from B's link events" + ("" if ok or rise else
                                              "; the index never rose: the modem's distortion measurement is a constant 0 "
                                              "(runtime.rs) or the overdrive did not reach the limiter")
        return row("distortion", PASS if ok else FAIL, f"rise in {rise} frames, back to 0 in {clear} frames", "air", note, ev)

    def _resume(self) -> dict:
        cfg, a, b = self.cfg, self.conn["a"], self.conn["b"]
        out, status = {}, PASS
        for secs in (60, 120):
            m = a.mark()
            if self.status("a")["session"] is None:
                a.request("connect", peer=cfg.b.callsign, resume=False)
                a.wait(is_state("open"), 90, m)
            path, sha = self.gen_file("a", 60, f"resume{secs}")
            mb = b.mark()
            a.request("send", file=path)
            a.wait(lambda e: e.get("event") == "session" and e.get("kind") == "progress"
                   and e["progress"]["tx"]["acked_bytes"] > 8000, 120, m)
            self.op.ask(f"SWITCH OFF radio B (or disconnect its antenna) NOW; I time {secs} s and then ask you to restore it")
            t_off = time.monotonic()
            while time.monotonic() - t_off < secs:
                time.sleep(1)
            print(f"\a>>> RESTORE radio B now ({secs} s have passed)", flush=True)
            self.op.ask("restore radio B (power on / reconnect), then press Enter")
            actual = time.monotonic() - t_off
            lost = a.select(is_state("link_lost"), m)
            resumed = None
            if lost or self.status("a")["session"] is None:
                m2 = a.mark()
                a.request("connect", peer=cfg.b.callsign, resume=True)
                o = a.wait(is_state("open"), 120, m2)
                resumed = bool(o and o.get("resumed"))
            done = a.wait(lambda e: e.get("event") == "session" and e.get("kind") in ("delivered", "undelivered"), 400, m)
            saved = b.wait(lambda e: e.get("event") == "rx" and (e.get("saved") or {}).get("complete"), 30, mb)
            undelivered = bool(a.select(lambda e: e.get("event") == "session" and e.get("kind") == "undelivered", m))
            match = (saved or {}).get("saved", {}).get("sha256") == sha
            ok = bool(done and done["kind"] == "delivered" and match and not undelivered)
            if secs == 60:
                ok = ok and not lost
            else:
                ok = ok and bool(lost) and bool(resumed)
            out[secs] = {"outage_s": round(actual, 1), "link_lost": bool(lost), "resumed": resumed, "delivered": bool(done),
                         "sha256_match": match, "undelivered": undelivered, "ok": ok}
            status = status if ok else FAIL
            if self.status("a")["session"] is not None:
                a.request("disconnect", graceful=False)
                time.sleep(3)
        meas = "; ".join(f"{s} s outage: lost={v['link_lost']} resumed={v['resumed']} sha_match={v['sha256_match']}"
                         for s, v in out.items())
        return row("resume", status, meas, "air",
                   "no block lost or delivered twice = delivered with a matching sha256; blocks re-sent after an "
                   "acknowledgement are not visible in the API (follow-up)", out)

    def _deference(self) -> dict:
        cfg = self.cfg
        if not cfg.afsk_source_cmd:
            return row("deference", MANUAL, "", "air", "no afsk_source_cmd configured: needs a Bell 202 source")
        a = self.conn["a"]
        if self.status("a")["session"] is None:
            a.request("connect", peer=cfg.b.callsign)
            a.wait(is_state("open"), 60, a.mark())
        path, _ = self.gen_file("a", 400, "defer")
        m = a.mark()
        a.request("send", file=path)
        for _ in range(8):
            time.sleep(12)
            subprocess.run(shlex.split(cfg.afsk_source_cmd), timeout=60, stdin=subprocess.DEVNULL)
        dcd = [{"at": e["at"], "afsk": e["detectors"]["afsk"]} for e in
               a.select(lambda e: e.get("event") == "dcd", m)]
        starts = [e["modem_time"] for e in a.select(lambda e: e.get("event") == "tx" and e.get("kind") == "start", m)]
        a.request("disconnect", graceful=False)
        time.sleep(3)
        bursts, bad = afsk_violations(dcd, starts)
        st = INCONCLUSIVE if bursts < 5 else (PASS if bad == 0 else FAIL)
        return row("deference", st, f"{bursts} Bell 202 bursts detected, {bad} transmissions started inside one (+100 ms)",
                   "air", "needs at least 5 detected bursts", {"bursts": bursts, "violations": bad})


# --------------------------------------------------------------------------------------
# The procedure
# --------------------------------------------------------------------------------------

def frame_points(cfg: Config, quick: bool, simulate: bool) -> list[dict]:
    """Every frame run the procedure makes: row, rung, SNR, direction, count. Also the basis of the printed plan."""
    n = 3 if quick else cfg.frames
    pts: list[dict] = []
    for d in ("a2b", "b2a"):
        pts.append({"row": "min_link", "mcs": 0, "snr": 8.0, "dir": d, "count": n})
    for d in ("a2b", "b2a"):
        for m in range(7):
            pts.append({"row": "bler_switch", "mcs": m, "snr": THRESHOLD_DB[m] + cfg.switch_margin_db, "dir": d, "count": n})
    pts.sort(key=lambda p: (p["snr"], p["dir"]))  # both directions at one attenuator setting, settings ascending
    pts.append({"row": "detection", "mcs": 0, "snr": 6.0, "dir": "a2b", "count": 5 if quick else cfg.detection_frames})
    if simulate:
        for ppm in (0.0, 100.0, -100.0):
            pts.append({"row": "clock", "mcs": 4, "snr": THRESHOLD_DB[4] + 2.0, "dir": "a2b", "count": n, "ppm": ppm})
        pts.append({"row": "throughput", "mcs": 5, "snr": 25.0, "dir": "a2b", "count": n})
    return pts


def plan_text(cfg: Config, args, simulate: bool) -> tuple[str, dict]:
    """What will be transmitted, by which station. Returns the text and keyed seconds per station."""
    keyed = {"a": 0.0, "b": 0.0}
    lines = []
    for p in frame_points(cfg, args.quick, simulate):
        src = "a" if p["dir"] == "a2b" else "b"
        s = p["count"] * (FRAME_AIR_S[p["mcs"]] + 2.0)
        keyed[src] += s
        st = getattr(cfg, src)
        lines.append(f"  station {src.upper()} ({st.callsign}) keys {p['count']} x MCS {p['mcs']} UI frame(s) with the ID block "
                     f"({p['row']}, ~{p['count'] * FRAME_AIR_S[p['mcs']]:.0f} s on air)")
    if not simulate:
        sess = cfg.file_kb * 1024 * 8 / 5000 * 2 + 60 + 2 * 200 + 400
        keyed["a"] += sess * 0.6
        keyed["b"] += sess * 0.6
        lines.append(f"  both stations run `cairn-modem run` (session): link test probe, a {cfg.file_kb} kB file each way, "
                     "rate-adaptation stream (a 400 kB file queued), distortion stream, two resume transfers (60 kB); "
                     f"roughly {sess * 0.6:.0f} s keyed each, callsign in every header, ID block in CONNECT/DISCONNECT "
                     "and every 9 minutes")
        lines.append(f"  idle listen {cfg.fcd_s} s each (no transmission) for the false carrier-detect row")
    head = "Plan (" + ("simulation: nothing is transmitted" if simulate else "ON AIR") + "):\n"
    tot = f"Estimated keyed time: A {keyed['a']:.0f} s, B {keyed['b']:.0f} s (budget {cfg.max_keyed_s} s each)."
    return head + "\n".join(lines) + "\n" + tot, keyed


def procedure(lab: Lab, cfg: Config, args, simulate: bool, only: set[str] | None) -> tuple[list[dict], dict]:
    rows: dict[str, dict] = {}
    raw: dict = {"frame_runs": []}

    def want(r: str) -> bool:
        return only is None or r in only

    def run_points(row_id: str, pts: list[dict]) -> list[FrameRun]:
        out = []
        for p in pts:
            if p["row"] != row_id:
                continue
            if p["snr"] is not None:
                lab.reach_snr(p["dir"], p["snr"])
            fr = lab.frame_run(p["dir"], p["mcs"], p["count"], p["snr"], ppm=p.get("ppm", 0.0))
            if fr is None:
                continue
            fr.ppm_forced = p.get("ppm", 0.0)  # type: ignore[attr-defined]
            out.append(fr)
            raw["frame_runs"].append({**fr.summary(), "row": row_id, "ppm_forced": p.get("ppm", 0.0)})
        return out

    pts = frame_points(cfg, args.quick, simulate)
    if want("min_link"):
        rows["min_link"] = eval_min_link(run_points("min_link", pts))
    if want("bler_switch"):
        rows["bler_switch"] = eval_bler_switch(run_points("bler_switch", pts), cfg.switch_margin_db)
    if want("detection"):
        runs = run_points("detection", pts)
        rows["detection"] = eval_detection(runs[0] if runs else None)
    if want("clock"):
        runs = run_points("clock", pts)
        if simulate:
            rows["clock"] = eval_clock(runs, True)
        else:  # observed offsets over every frame run so far
            ppms = [p for fr in raw["frame_runs"] for p in (fr["ppm_range"] or [])]
            rows["clock"] = row("clock", INFO, f"observed {min(ppms):+.0f}..{max(ppms):+.0f} ppm over "
                                f"{len(raw['frame_runs'])} frame runs" if ppms else "no readings", "frames",
                                "the +/-100 ppm span cannot be forced on air; this is the pair's own offset") \
                if ppms else row("clock", NOT_RUN, "", "-", "no clock readings")
    if simulate and want("throughput"):
        runs = run_points("throughput", pts)
        if runs:
            r = runs[0]
            rows["throughput"] = row("throughput", PASS if r.net_kbps_model() >= 5.0 else FAIL,
                                     f"MCS 5 at {r.snr_mean or float('nan'):.1f} dB: BLER {r.bler * 100:.1f}%, net "
                                     f"{r.net_kbps_model():.2f} kbps (A.2 model)", "sim-frames",
                                     "simulation: rung 5 is what 10.1 picks at 25 dB; the session figure comes from the "
                                     "cargo proxy", {"frames": r.summary()})
    session_rows = [r for r in ("throughput", "adaptation", "width", "resume", "link_test", "fcd", "deference", "distortion")
                    if want(r)]
    if session_rows:
        for r in lab.session_rows(session_rows):
            if r["id"] == "throughput" and "throughput" in rows:  # keep both pieces of evidence
                rows["throughput"]["evidence"]["proxy"] = r
                if r["status"] == FAIL:
                    rows["throughput"]["status"] = FAIL
                continue
            rows[r["id"]] = r
    for t in TARGETS:
        if t.id in NOT_MEASURABLE_ROWS and want(t.id):
            rows[t.id] = row(t.id, NOT_MEASURABLE, "", "-", t.how)
    for r in ("width",):
        if r in rows:
            rows[r]["notes"] = (rows[r]["notes"] + "; " if rows[r]["notes"] else "") + \
                "mask restore after an impairment: not measured (needs an injected single-tone impairment)"
    ordered = [rows[t.id] for t in TARGETS if t.id in rows]
    if "detection" in rows:
        ordered.append(rows["detection"])
    for t in TARGETS:
        if t.id not in rows and want(t.id):
            ordered.append(row(t.id, NOT_RUN if t.id not in ("deference",) else MANUAL, "", "-", "not run"))
    return ordered, raw


# --------------------------------------------------------------------------------------
# Output
# --------------------------------------------------------------------------------------

def summarize(rows: list[dict], simulated: bool) -> dict:
    cnt: dict = {}
    for r in rows:
        if r["id"] != "detection":
            cnt[r["status"]] = cnt.get(r["status"], 0) + 1
    outstanding = [r["id"] for r in rows if r["id"] != "detection" and r["status"] != PASS]
    return {"counts": cnt, "outstanding": outstanding,
            "all_measurable_rows_pass": not [r for r in rows if r["status"] in (FAIL, INCONCLUSIVE, NOT_RUN)
                                             and r["id"] != "detection"],
            "evidence_for_v0_5": False if simulated else None,
            "note": ("SIMULATION: not evidence for the v0.5 milestone (SPEC 16.2 needs two radio makes on air)." if simulated
                     else "One run is one pair of radios; merge runs for the milestone (--merge).")}


def markdown(res: dict) -> str:
    c = res["config"]
    out = [f"# SPEC 14.1 24-tone bench results ({'SIMULATION' if res['simulated'] else 'on air'})", ""]
    if res["simulated"]:
        out += ["> **Simulation.** These numbers come from the simulator and the reference modem, not from radios. They show "
                "the script works; they are not evidence for the v0.5 milestone.", ""]
    out += [f"- Started: {res['started']}  (script {res['script_rev']})",
            f"- Station A: {c['a']['callsign']}  make: {c['a']['make'] or '-'}  device: {c['a']['device']}",
            f"- Station B: {c['b']['callsign']}  make: {c['b']['make'] or '-'}  device: {c['b']['device']}"
            f"{'  (ssh)' if c['b']['ssh'] else ''}",
            f"- Rig pair: {c['rig_pair'] or '-'}; TXDelay {c['txdelay_ms']} ms, TXTail {c['txtail_ms']} ms, cap {c['cap']} tones",
            f"- Frequency / path notes: {c['frequency_notes'] or '-'} / {c['path_notes'] or '-'}",
            f"- Channel check: {'busy' if res['channel_check'].get('busy') else 'clear'} ({res['channel_check']})",
            "", "| Row (SPEC 14.1) | Target (24 tones) | Measured | Method | Result | Notes |", "| --- | --- | --- | --- | --- | --- |"]
    esc = lambda s: str(s).replace("|", "/").replace("\n", " ")  # noqa: E731
    for r in res["rows"]:
        out.append(f"| {esc(r['row'])} | {esc(r['target'])} | {esc(r['measured'])} | {r['method']} | **{r['status']}** | "
                   f"{esc(r['notes'])} |")
    s = res["summary"]
    out += ["", f"Counts: {s['counts']}. Rows not PASS: {', '.join(s['outstanding']) or 'none'}.", "", s["note"], "",
            "How each row is measured (and why some cannot be):", ""]
    for t in TARGETS:
        out.append(f"- **{t.id}**: {t.how}")
    out.append(f"- **detection** (extra): {EXTRA_DETECTION.how}")
    return "\n".join(out) + "\n"


def script_rev() -> str:
    try:
        return subprocess.run(["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"], capture_output=True, text=True,
                              timeout=10).stdout.strip() or "unknown"
    except (OSError, subprocess.SubprocessError):
        return "unknown"


def merge(paths: list[str], out: str | None) -> int:
    runs = [json.loads(Path(p).read_text()) for p in paths]
    if any(r.get("simulated") for r in runs):
        print("refusing: a simulated results file is not evidence", file=sys.stderr)
        return 2
    makes = {m.strip().lower() for r in runs for m in (r["config"]["a"].get("make", ""), r["config"]["b"].get("make", "")) if m.strip()}
    ids = [t.id for t in TARGETS]
    lines = ["# SPEC 14.1 24-tone targets across runs", "", f"Runs: {len(runs)}; radio makes named: {sorted(makes) or 'none'} "
             f"({len(makes)}); milestone needs at least 2.", "", "| Row | " + " | ".join(
                 f"run {i + 1} ({r['config']['rig_pair'] or r['config']['a']['make'] or '-'})" for i, r in enumerate(runs)) + " | Overall |",
             "| --- |" + " --- |" * (len(runs) + 1)]
    met = len(makes) >= 2
    for i in ids:
        cells = []
        for r in runs:
            m = next((x for x in r["rows"] if x["id"] == i), None)
            cells.append(m["status"] if m else "-")
        ok = all(c in (PASS, INFO) for c in cells)
        manual = all(c in (NOT_MEASURABLE, MANUAL) for c in cells)
        overall = "PASS" if ok else ("not measurable on air" if manual else "OPEN")
        met = met and (ok or manual)
        lines.append(f"| {TARGET_BY_ID[i].row} | " + " | ".join(cells) + f" | {overall} |")
    lines += ["", "Rows marked 'not measurable on air' need another procedure before the milestone can be closed "
              "(see the results files for why).",
              f"All measurable rows pass on >= 2 makes: **{'yes' if met else 'no'}**"]
    text = "\n".join(lines) + "\n"
    if out:
        Path(out).write_text(text)
    print(text)
    return 0


# --------------------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------------------

def parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--config", help="TOML config (see docs/bench-testing.md): [run], [station.a], [station.b]")
    ap.add_argument("--simulate", action="store_true", help="run the procedure against the simulator; nothing is keyed")
    ap.add_argument("--i-have-permission-to-transmit", dest="permission", action="store_true",
                    help="required for on-air mode: you hold a licence for these stations, frequency and power, and the "
                         "radios are into dummy loads or on a frequency you may use")
    ap.add_argument("--no-confirm", action="store_true", help="on air: skip the typed confirmation after the plan")
    ap.add_argument("--plan-only", action="store_true", help="print what would be transmitted and exit (keys nothing)")
    ap.add_argument("--list-targets", action="store_true", help="print every 14.1 row with its procedure and exit")
    ap.add_argument("--only", help="comma-separated row ids to run (see --list-targets)")
    ap.add_argument("--quick", action="store_true", help="few frames per point (a smoke test)")
    ap.add_argument("--out-dir", help="results directory (default bench-14-1-results/<timestamp>)")
    ap.add_argument("--non-interactive", action="store_true", help="never prompt (operator-driven rows then fail)")
    ap.add_argument("--merge", nargs="+", metavar="RESULTS", help="combine results.json files into the milestone table")
    ap.add_argument("--modem", help="cairn-modem binary for this machine (default: modem/target/release/cairn-modem)")
    ap.add_argument("--sim-profile", default="mic", help="simulated channel profile (default mic)")
    ap.add_argument("--sim-proxies", choices=["none", "quick", "all"], default="all",
                    help="simulation: which two_station cargo tests stand in for the session rows (default all, ~1 min)")
    ap.add_argument("--frames", type=int, help="frames per point (default: config, 10)")
    ap.add_argument("--fcd-s", type=int, help="idle seconds for the false carrier-detect row (default 600)")
    ap.add_argument("--channel-check-s", type=int, help="listen-only channel check, seconds (default 60)")
    return ap


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        return _main(args)
    except BenchError as e:
        print(f"bench_14_1: {e}", file=sys.stderr)
        return 2


def _main(args) -> int:
    if args.merge:
        if args.out_dir:
            Path(args.out_dir).mkdir(parents=True, exist_ok=True)
        return merge(args.merge, args.out_dir and str(Path(args.out_dir) / "milestone.md"))
    if args.list_targets:
        for t in [*TARGETS, EXTRA_DETECTION]:
            print(f"{t.id:15} {t.row}\n{'':15} target: {t.target}\n{'':15} how:    {t.how}\n")
        return 0
    simulate = args.simulate
    cfg = load_config(args.config, {"frames": args.frames, "fcd_s": args.fcd_s, "channel_check_s": args.channel_check_s})
    if simulate and not args.config:
        cfg.fcd_s = min(cfg.fcd_s, 600)
    only = set(args.only.split(",")) if args.only else None
    if only:
        bad = only - set(TARGET_BY_ID) - {"detection"}
        if bad:
            raise BenchError(f"unknown row ids {sorted(bad)} (see --list-targets)")
    check_callsigns(cfg, on_air=not simulate)
    text, keyed = plan_text(cfg, args, simulate)
    print(text)
    if args.plan_only:
        return 0
    if not simulate:
        if not args.permission:
            raise BenchError("on-air mode keys transmitters. Add --i-have-permission-to-transmit once you are sure you may "
                             "(licence, frequency, power, dummy load or a clear channel), or use --simulate, or --plan-only.")
        for st in (cfg.a, cfg.b):
            if keyed[st.name] > cfg.max_keyed_s:
                raise BenchError(f"station {st.name.upper()}: plan needs ~{keyed[st.name]:.0f} s keyed, over max_keyed_s "
                                 f"{cfg.max_keyed_s}; use --only or raise max_keyed_s")
        if not args.no_confirm:
            if not sys.stdin.isatty():
                raise BenchError("confirmation needs a terminal (or pass --no-confirm)")
            ans = input(f"\nType the callsign of station A ({cfg.a.callsign}) to go on the air, anything else to stop: ")
            if ans.strip().upper() != cfg.a.callsign.strip().upper():
                raise BenchError("not confirmed; nothing was transmitted")
    out = Path(args.out_dir) if args.out_dir else Path("bench-14-1-results") / dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    out = out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    work = out / "logs"
    work.mkdir(exist_ok=True)
    modem = Path(args.modem) if args.modem else DEFAULT_MODEM
    if simulate:
        if not modem.exists():
            raise BenchError(f"{modem} not found: cargo build --release in modem/")
        lab: Lab = SimLab(cfg, work, modem, args.sim_profile, "quick" if args.quick and args.sim_proxies == "all" else args.sim_proxies)
    else:
        lab = AirLab(cfg, work, Operator(not args.non_interactive), cfg.a.modem or str(modem),
                     cfg.b.modem or ("cairn-modem" if cfg.b.ssh else str(modem)))
    started = dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds")
    try:
        info = lab.preflight()
        check = lab.channel_check()
        if check.get("busy"):
            (out / "results.json").write_text(json.dumps({"schema": SCHEMA, "aborted": "channel busy",
                                                           "channel_check": check}, indent=2))
            raise BenchError(f"the channel is busy ({check}); nothing was transmitted. Try again when it is clear.")
        rows, raw = procedure(lab, cfg, args, simulate, only)
    finally:
        lab.close()
    res = {"schema": SCHEMA, "simulated": simulate, "started": started, "script_rev": script_rev(), "preflight": info,
           "channel_check": check, "config": {"a": vars(cfg.a), "b": vars(cfg.b), **{k: getattr(cfg, k) for k in (
               "txdelay_ms", "txtail_ms", "cap", "frequency_notes", "path_notes", "rig_pair", "attenuator", "frames",
               "file_kb", "fcd_s", "switch_margin_db")}},
           "keyed_s_estimate_per_station": {k: round(v) for k, v in keyed.items()},
           "rows": rows, "summary": summarize(rows, simulate), "raw": raw}
    (out / "results.json").write_text(json.dumps(res, indent=2, default=str))
    (out / "results.md").write_text(markdown(res))
    print((out / "results.md").read_text())
    print(f"wrote {out / 'results.json'} and {out / 'results.md'}")
    return 0 if not any(r["status"] == FAIL for r in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
