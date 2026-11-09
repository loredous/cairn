"""modem/tools/bench_14_1.py: the SPEC 14.1 bench script.

Nothing here keys a transmitter or opens a sound card: every on-air test checks that the script refuses
(and makes subprocess unusable to prove it), and the end-to-end test runs `--simulate`.
"""

from __future__ import annotations

import importlib.util
import json
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "modem" / "tools" / "bench_14_1.py"
MODEM = ROOT / "modem" / "target" / "release" / "cairn-modem"


@pytest.fixture(scope="module")
def bench():
    spec = importlib.util.spec_from_file_location("bench_14_1", SCRIPT)
    mod = importlib.util.module_from_spec(spec)
    sys.modules["bench_14_1"] = mod
    spec.loader.exec_module(mod)
    return mod


def write_cfg(tmp_path: Path, a="K1ABC-1", b="K1ABC-2", extra="") -> str:
    p = tmp_path / "bench.toml"
    p.write_text(f"""
[run]
txdelay_ms = 300
{extra}
[station.a]
callsign = "{a}"
device = "All-In-One-Cable"
ptt = "serial:All-In-One-Cable"
make = "Make One"
[station.b]
callsign = "{b}"
device = "All-In-One-Cable"
ptt = "serial:All-In-One-Cable"
ssh = "bench-b"
make = "Make Two"
""")
    return str(p)


@pytest.fixture
def no_processes(monkeypatch, bench):
    """Any attempt to start a process (so, to touch hardware or ssh) fails the test."""
    def boom(*a, **k):
        raise AssertionError(f"started a process: {a[:1]}")
    monkeypatch.setattr(bench.subprocess, "Popen", boom)
    monkeypatch.setattr(bench.subprocess, "run", boom)
    monkeypatch.setattr(bench.socket, "create_connection", boom)


# ---- safety gates -------------------------------------------------------------------------

@pytest.mark.parametrize("call", ["N0CALL-1", "NOCALL", "mycall-3", "TEST"])
def test_on_air_refuses_placeholder_callsigns(bench, tmp_path, capsys, no_processes, call):
    cfg = write_cfg(tmp_path, a=call)
    rc = bench.main(["--config", cfg, "--i-have-permission-to-transmit", "--no-confirm"])
    assert rc == 2
    assert "placeholder" in capsys.readouterr().err


def test_on_air_refuses_malformed_and_identical_callsigns(bench, tmp_path, capsys, no_processes):
    assert bench.main(["--config", write_cfg(tmp_path, a="K1"), "--i-have-permission-to-transmit"]) == 2
    assert bench.main(["--config", write_cfg(tmp_path, a="K1ABC-1", b="K1ABC-1"), "--i-have-permission-to-transmit"]) == 2
    assert "must differ" in capsys.readouterr().err


def test_on_air_needs_the_permission_flag(bench, tmp_path, capsys, no_processes):
    rc = bench.main(["--config", write_cfg(tmp_path), "--no-confirm"])
    out = capsys.readouterr()
    assert rc == 2
    assert "--i-have-permission-to-transmit" in out.err
    assert "ON AIR" in out.out  # the plan was shown first


def test_on_air_needs_the_typed_confirmation(bench, tmp_path, capsys, no_processes, monkeypatch):
    monkeypatch.setattr(sys.stdin, "isatty", lambda: True)
    monkeypatch.setattr("builtins.input", lambda *_: "no")
    rc = bench.main(["--config", write_cfg(tmp_path), "--i-have-permission-to-transmit"])
    assert rc == 2
    assert "not confirmed" in capsys.readouterr().err


def test_keyed_time_budget_is_enforced_before_keying(bench, tmp_path, capsys, no_processes):
    cfg = write_cfg(tmp_path, extra="max_keyed_s = 60")
    rc = bench.main(["--config", cfg, "--i-have-permission-to-transmit", "--no-confirm"])
    assert rc == 2
    assert "max_keyed_s" in capsys.readouterr().err


def test_plan_only_names_each_station_and_keys_nothing(bench, tmp_path, capsys, no_processes):
    rc = bench.main(["--config", write_cfg(tmp_path), "--plan-only"])
    out = capsys.readouterr().out
    assert rc == 0
    assert "station A (K1ABC-1) keys" in out and "station B (K1ABC-2) keys" in out
    assert "Estimated keyed time" in out and "ID block" in out


def test_n0call_is_fine_in_simulation_only(bench):
    cfg = bench.load_config(None, {})
    assert cfg.a.callsign == "N0CALL-1"
    bench.check_callsigns(cfg, on_air=False)
    with pytest.raises(bench.BenchError):
        bench.check_callsigns(cfg, on_air=True)


def test_unknown_config_keys_are_rejected(bench, tmp_path):
    with pytest.raises(bench.BenchError):
        bench.load_config(write_cfg(tmp_path, extra="txdealy_ms = 1"), {})


def test_every_14_1_row_is_mapped(bench):
    spec = (ROOT / "docs" / "SPEC.md").read_text()
    rows = [line.split("|")[1].strip() for line in spec.split("### 14.1 Targets")[1].split("The adaptation target")[0].splitlines()
            if line.startswith("| ") and not line.startswith("| Target") and not line.startswith("| ---")]
    assert len(rows) == len(bench.TARGETS) == 16
    # each script row is the first words of a SPEC row
    for t in bench.TARGETS:
        assert any(r.lower().startswith(t.row.split(",")[0].lower()[:12]) for r in rows), t.row


# ---- parsing and evaluation ------------------------------------------------------------------

LISTEN = """\
config: from each header
[   1.001 s] K1ABC-1 > *  UI [first]  seq 0  MCS 4 x24 short prefix  blocks 28/28  ctrl 0/0  SNR  21.1 dB  clock  +50.4 ppm  rho 0.93  decode 11 ms
[   5.292 s] K1ABC-1 > *  UI [first]  seq 1  MCS 4 x24 short prefix  blocks 20/28  ctrl 0/0  SNR  20.9 dB  clock  +49.8 ppm  rho 0.95  decode 13 ms
[   9.583 s] not decoded: header did not decode
done: 14.4 s of audio in 0.3 s (49x real time); detections 3, frames 3, headers ok 2, blocks 48/84, worst decode latency 13 ms
"""


def test_parse_listen_and_score(bench):
    p = bench.parse_listen(LISTEN)
    assert len(p["frames"]) == 2 and p["header_failures"] == 1
    fr = bench.frame_run_from_listen(p, direction="a2b", mcs=4, sent=3, src="K1ABC-1", target_snr=20.0)
    assert (fr.decoded, fr.blocks_ok, fr.blocks_sent) == (2, 48, 84)  # the lost frame loses all 28 blocks
    assert fr.bler == pytest.approx(36 / 84)
    assert fr.net_kbps_model() == pytest.approx(6.09 * 48 / 84)
    assert fr.ppm == [50.4, 49.8]


def test_other_callsigns_and_rungs_do_not_count(bench):
    p = bench.parse_listen(LISTEN)
    assert bench.frame_run_from_listen(p, direction="a2b", mcs=4, sent=3, src="W9XYZ-1").decoded == 0
    assert bench.frame_run_from_listen(p, direction="a2b", mcs=2, sent=3, src="K1ABC-1").decoded == 0


def test_rung_helpers(bench):
    assert [bench.rung_for_snr(s) for s in (0, 5.5, 10.5, 23.5, 40)] == [0, 0, 2, 5, 6]
    assert bench.count_reversals([4, 4, 3, 3, 4, 4]) == 1
    assert bench.count_reversals([2, 3, 4, 5]) == 0
    assert bench.frames_to_settle([5, 5, 4, 3, 3, 3, 3, 3]) == (4, 3)
    assert bench.frames_to_settle([5, 4, 3, 4, 3, 3, 3, 3]) == (5, 3)
    assert bench.frames_to_settle([]) == (None, None)
    assert bench.frames_until([0, 0, 3, 5], lambda v: v >= 3) == 3


def test_afsk_deference_check(bench):
    dcd = [{"at": 10.0, "afsk": True}, {"at": 10.8, "afsk": False}, {"at": 30.0, "afsk": True}, {"at": 30.5, "afsk": False}]
    assert bench.afsk_violations(dcd, [5.0, 12.0]) == (2, 0)
    assert bench.afsk_violations(dcd, [10.85]) == (2, 1)  # within 100 ms after the burst ends
    assert bench.afsk_violations(dcd, [10.5]) == (2, 1)


def test_verdicts_follow_the_targets(bench):
    def fr(mcs, bler_blocks, snr, target):
        n = bench.A2_BLOCKS[mcs] * 4
        return bench.FrameRun("a2b", mcs, 4, 4, n - bler_blocks, n, [snr] * 4, [], 0, target)
    assert bench.eval_min_link([fr(0, 0, 8.3, 8.0)])["status"] == bench.PASS
    assert bench.eval_min_link([fr(0, 1, 8.3, 8.0)])["status"] == bench.FAIL  # 1.21 kbps leaves no room for a lost block
    assert bench.eval_min_link([fr(0, 0, 14.0, 8.0)])["status"] == bench.INCONCLUSIVE
    ok = [fr(m, 0, bench.THRESHOLD_DB[m] + 2, bench.THRESHOLD_DB[m] + 2) for m in range(7)]
    assert bench.eval_bler_switch(ok, 2.0)["status"] == bench.PASS
    assert bench.eval_bler_switch([*ok, fr(6, 20, 27.5, 27.5)], 2.0)["status"] == bench.FAIL  # 20/160 blocks lost: 12.5%
    assert bench.eval_bler_switch([fr(3, 0, 20.0, 14.5)], 2.0)["status"] == bench.INCONCLUSIVE


def test_detection_needs_a_hundred_frames_to_pass_or_fail(bench):
    def run(n, decoded):
        return bench.FrameRun("a2b", 0, n, decoded, decoded * 3, n * 3, [7.0] * decoded, [], 0, 6.0)
    assert bench.eval_detection(run(30, 29))["status"] == bench.INFO  # one miss in 30 is what 1% often gives
    assert bench.eval_detection(run(30, 25))["status"] == bench.FAIL
    assert bench.eval_detection(run(100, 99))["status"] == bench.PASS
    assert bench.eval_detection(run(200, 190))["status"] == bench.FAIL
    assert bench.eval_detection(run(10, 10))["status"] == bench.INFO


# ---- merging runs ----------------------------------------------------------------------------

def result_file(bench, tmp_path, name, make_a, make_b, status, simulated=False):
    rows = [{"id": t.id, "status": status if t.id not in ("drive_cal", "interop") else "NOT_MEASURABLE"}
            for t in bench.TARGETS]
    cfg = {"a": {"make": make_a}, "b": {"make": make_b}, "rig_pair": name}
    p = tmp_path / f"{name}.json"
    p.write_text(json.dumps({"simulated": simulated, "config": cfg, "rows": rows}))
    return str(p)


def test_merge_needs_two_makes_and_refuses_simulation(bench, tmp_path, capsys):
    one = result_file(bench, tmp_path, "one", "Make One", "Make Two", "PASS")
    assert bench.merge([one], None) == 0
    out = capsys.readouterr().out
    assert "['make one', 'make two'] (2)" in out and "**yes**" in out
    same_make = result_file(bench, tmp_path, "mono", "Make One", "Make One", "PASS")
    bench.merge([same_make], None)
    assert "**no**" in capsys.readouterr().out  # one make is not the milestone
    failing = result_file(bench, tmp_path, "two", "Make One", "Make Three", "FAIL")
    bench.merge([one, failing], None)
    assert "**no**" in capsys.readouterr().out
    sim = result_file(bench, tmp_path, "sim", "x", "y", "PASS", simulated=True)
    assert bench.merge([one, sim], None) == 2


# ---- the simulated run ---------------------------------------------------------------------

@pytest.mark.skipif(not MODEM.exists() or not shutil.which("cargo"), reason="needs `cargo build --release` in modem/")
def test_simulated_run_end_to_end(bench, tmp_path):
    out = tmp_path / "res"
    cp = subprocess.run([sys.executable, str(SCRIPT), "--simulate", "--quick", "--out-dir", str(out)],
                        capture_output=True, text=True, timeout=600)
    assert cp.returncode == 0, cp.stdout[-2000:] + cp.stderr[-2000:]
    res = json.loads((out / "results.json").read_text())
    assert res["schema"] == "cairn-bench-14.1/1" and res["simulated"] is True
    assert res["summary"]["evidence_for_v0_5"] is False
    rows = {r["id"]: r for r in res["rows"]}
    assert set(rows) == {t.id for t in bench.TARGETS} | {"detection"}
    for rid in ("min_link", "bler_switch", "clock", "throughput", "fcd"):
        assert rows[rid]["status"] == "PASS", rows[rid]
    assert rows["detection"]["status"] in ("PASS", "INFO")  # five frames cannot show 99%
    for rid in ("drive_cal", "repeater", "soft_combining", "multimode", "interop"):
        assert rows[rid]["status"] == "NOT_MEASURABLE" and rows[rid]["notes"]
    assert rows["fcd"]["method"] == "sim-proxy"
    assert not any(r["status"] == "FAIL" for r in rows.values())
    md = (out / "results.md").read_text()
    assert "SIMULATION" in md and "| Row (SPEC 14.1) |" in md and "**PASS**" in md
    assert res["channel_check"]["busy"] is False
