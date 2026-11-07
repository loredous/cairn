"""Bench test kit: tx -> (channel, damage) -> rx, and the device helpers with mocks."""

from __future__ import annotations

import json
import signal

import numpy as np
import pytest
from scipy.io import wavfile

from cairn_sim import channel, params
from cairn_sim.bench import audio, cli, devices, rx, tx

FS = params.FS


@pytest.fixture(scope="module")
def kit():
    specs = [tx.FrameSpec(0, 24, blocks=2), tx.FrameSpec(2, 24, blocks=2),
             tx.FrameSpec(4, 32, blocks=2), tx.FrameSpec(0, 24, "long", blocks=2),
             tx.FrameSpec(2, 40, silenced=(8, 9, 10), blocks=2)]
    x, manifest = tx.synthesize(specs, tone_s=1.5, callsign="N0CALL", lead_s=0.5)
    return x, manifest


def run(y, manifest, **kw):
    return rx.analyze(y, manifest, **kw)


def assert_all_decoded(rep, n):
    s = rep["summary"]
    assert s["missed"] == [] and s["detected"] == n and s["extra_detections"] == 0
    assert s["frames_all_blocks_ok"] == n and s["blocks_ok"] == s["blocks_sent"]
    assert all(r["header_match"] and r["prefix_ok"] for r in rep["frames"])


# --- tx -----------------------------------------------------------------------


def test_tx_levels_and_manifest(kit):
    x, m = kit
    peak = 10 ** (params.PEAK_DBFS / 20)
    assert np.abs(x).max() <= peak + 1e-9
    for f in m["frames"]:
        seg = x[f["start_sample"]:f["start_sample"] + f["n_samples"]]
        assert np.isclose(np.abs(seg).max(), peak)
    t = m["level_tone"]
    tone = x[int(t["start_s"] * FS):int((t["start_s"] + t["duration_s"]) * FS)]
    assert np.isclose(np.abs(tone).max(), peak, rtol=1e-3)
    assert [f["seed"] for f in m["frames"]] == sorted({f["seed"] for f in m["frames"]})
    assert m["frames"][3]["cp"] == params.CP_LONG and m["frames"][4]["silenced"] == [8, 9, 10]
    json.dumps(m)


def test_plans():
    assert [s.mcs for s in tx.make_plan("ladder24")] == list(range(7))
    assert [s.width for s in tx.make_plan("width")] == [24, 32, 40, 48, 56]
    assert len(tx.make_plan("custom", rungs=(0, 1), widths=(24, 40), prefix="both")) == 8
    assert tx.parse_tones("8-11,30") == (8, 9, 10, 11, 30)
    assert tx.FrameSpec(0, 24).n_blocks() >= 1
    with pytest.raises(ValueError):
        tx.make_plan("nope")


def test_cw_id_at_start_end_and_every_ten_minutes():
    specs = [tx.FrameSpec(0, 24, blocks=1)]
    x, m = tx.synthesize(specs, repeat=4, tone_s=0, gap_s=200.0, callsign="K1ABC", lead_s=0.5)
    ids = [c["start_s"] for c in m["cw_ids"]]
    assert ids[0] < 1 and ids[-1] > m["frames"][-1]["start_s"]
    marks = sorted(ids + [m["duration_s"]])
    assert all(b - a <= 600 for a, b in zip(marks, marks[1:]))
    assert len(ids) >= 3
    cw = audio.morse("E", 20, 800, 0.7)               # one dit, 60 ms
    assert abs(len(cw) / FS - 0.06) < 1e-3
    first = m["cw_ids"][0]
    seg = x[int(first["start_s"] * FS):int((first["start_s"] + first["duration_s"]) * FS)]
    assert np.isclose(np.abs(seg).max(), 10 ** (params.PEAK_DBFS / 20), rtol=1e-3)


# --- rx -----------------------------------------------------------------------


def test_round_trip_clean_through_wav_and_cli(kit, tmp_path, capsys):
    x, m = kit
    wav = tmp_path / "t.wav"
    tx.write(str(wav), str(tmp_path / "t.json"), x, m)
    rc = cli.main(["rx", str(wav), "--json", str(tmp_path / "r.json"), "--plots", str(tmp_path / "p")])
    assert rc == 0
    out = capsys.readouterr().out
    assert "MISSED" not in out and "Summary" in out
    rep = json.loads((tmp_path / "r.json").read_text())
    assert_all_decoded(rep, 5)
    assert rep["level_tone"]["found"] and abs(rep["level_tone"]["peak_dbfs"] + 3) < 0.1
    assert (tmp_path / "p" / "snr_per_tone.png").exists()
    assert list((tmp_path / "p").glob("constellation_*.png"))
    f = rep["frames"][2]
    assert len(f["snr_per_tone_db"]) == 32 and f["distortion"]["index"] <= 1
    assert f["adaptation"]["first_frame"] >= 0


def test_cli_tx_writes_files(tmp_path, capsys):
    wav = tmp_path / "q.wav"
    assert cli.main(["tx", "--plan", "quick", "-o", str(wav), "--blocks", "1", "--tone-s", "0.5"]) == 0
    fs, data = wavfile.read(wav)
    assert fs == FS and data.ndim == 1 and data.dtype == np.int16
    assert len(json.loads((tmp_path / "q.json").read_text())["frames"]) == 3


def test_fm_channel_mic_comfortable_snr(kit):
    x, m = kit
    y = channel.FmChannel("mic", snr_db=30.0, width=24)(x, np.random.default_rng(5)) * 0.3
    rep = run(y, m)
    assert rep["summary"]["missed"] == []
    assert rep["summary"]["headers_ok"] == 5
    for r in rep["frames"]:
        assert r["blocks_ok"] == r["blocks_total"], r
    assert 15 < rep["frames"][0]["snr7_db"] < 45


def test_fm_channel_low_snr_reports_errors_honestly(kit):
    x, m = kit
    y = channel.FmChannel("mic", snr_db=8.0, width=24)(x, np.random.default_rng(6)) * 0.3
    rep = run(y, m)
    s = rep["summary"]
    assert s["frames_all_blocks_ok"] < s["frames_sent"]
    assert s["blocks_ok"] < s["blocks_sent"]
    for r in rep["frames"]:
        if r["status"] == "decoded":
            assert r["blocks_ok"] == sum(r["block_ok"])
            if r["blocks_ok"] < r["blocks_total"]:
                assert max(r["block_bit_errors"]) > 0
    assert len(s["missed"]) + s["detected"] == s["frames_sent"]


@pytest.mark.parametrize("ppm", [200.0, -200.0])
def test_clock_offset(kit, ppm):
    x, m = kit
    rep = run(channel.resample(x, 1 + ppm * 1e-6), m)
    assert_all_decoded(rep, 5)
    assert abs(rep["summary"]["clock_ppm_global"] - ppm) < 5
    for r in rep["frames"]:
        assert abs(r["clock_ppm"] - ppm) < 15


def test_44k1_recording(kit, tmp_path):
    x, m = kit
    from scipy import signal
    y = signal.resample_poly(x, 147, 160)
    wavfile.write(tmp_path / "r.wav", 44100, np.rint(y * 32767).astype(np.int16))
    sig, info = rx.load(str(tmp_path / "r.wav"))
    assert info["sample_rate_in"] == 44100 and abs(len(sig) - len(x)) <= 2
    assert_all_decoded(run(sig, m), 5)


def test_leading_noise_and_low_level(kit):
    x, m = kit
    rng = np.random.default_rng(7)
    level = 10 ** (-25 / 20) / np.abs(x).max()
    y = np.concatenate([0.01 * rng.standard_normal(2 * FS), x * level, 0.01 * rng.standard_normal(FS)])
    rep = run(y, m)
    assert_all_decoded(rep, 5)
    assert abs(rep["frames"][0]["time_s"] - (2.0 + m["frames"][0]["start_s"])) < 0.01


def test_removed_frame_is_reported_missed(kit):
    x, m = kit
    f = m["frames"][2]
    y = x.copy()
    y[f["start_sample"] - 100:f["start_sample"] + f["n_samples"] + 100] = 0.0
    rep = run(y, m)
    s = rep["summary"]
    assert s["missed"] == [2] and s["detected"] == 4 and s["extra_detections"] == 0
    assert rep["frames"][2]["status"] == "missed"
    assert s["blocks_ok"] == s["blocks_sent"] - f["n_blocks"]
    assert "MISSED" in rx.format_report({"recording": rx.recording_info(FS, 1, "mono", y), **rep})


def test_stereo_input(kit, tmp_path):
    x, m = kit
    rng = np.random.default_rng(8)
    stereo = np.stack([0.002 * rng.standard_normal(len(x)), x], axis=1)
    wavfile.write(tmp_path / "s.wav", FS, np.rint(stereo * 32767).astype(np.int16))
    sig, info = rx.load(str(tmp_path / "s.wav"))
    assert info["channels"] == 2 and "channel 1" in info["channel_used"]
    assert_all_decoded(run(sig, m), 5)
    left, _ = rx.load(str(tmp_path / "s.wav"), "left")
    assert np.abs(left).max() < 0.05


def test_rx_clipped_input_is_flagged(kit):
    x, m = kit
    rep = run(np.clip(x * 1.6, -1, 1), m)
    assert rep["summary"]["input_clipped"]


def test_match_frames_with_a_gap_and_drift():
    nominal = np.arange(10) * 5.0 + 3.0
    det = (nominal * 1.0003 + 7.1)[[0, 1, 2, 4, 5, 8, 9]] + 0.002
    pairs, off, slope = rx.match_frames(det, nominal)
    assert [pairs[i] for i in sorted(pairs)] == [0, 1, 2, 4, 5, 8, 9]
    assert abs(slope - 1.0003) < 1e-5


def test_adaptation_uses_6_2_thresholds():
    assert rx.adaptation(30.0) == {"first_frame": 6, "first_frame_below_mcs0": False, "supported": 6}
    assert rx.adaptation(12.0)["first_frame"] == 1 and rx.adaptation(12.0)["supported"] == 2
    assert rx.adaptation(2.0)["first_frame_below_mcs0"] and rx.adaptation(2.0)["supported"] is None


# --- devices (mocked) -----------------------------------------------------------


class FakeSerial:
    log: list = []

    def __init__(self):
        self.is_open = False
        self._dtr = self._rts = None
        self.port = None

    def open(self):
        FakeSerial.log.append(("open", self._dtr, self._rts))
        self.is_open = True

    def close(self):
        FakeSerial.log.append(("close",))
        self.is_open = False

    dtr = property(lambda s: s._dtr, lambda s, v: (setattr(s, "_dtr", v), FakeSerial.log.append(("dtr", v))))
    rts = property(lambda s: s._rts, lambda s, v: (setattr(s, "_rts", v), FakeSerial.log.append(("rts", v))))


class FakeSerialModule:
    Serial = FakeSerial


@pytest.fixture
def fake_serial():
    FakeSerial.log = []
    return FakeSerialModule


def test_ptt_lines_low_before_open_and_default_keys_dtr(fake_serial):
    with devices.Ptt("/dev/fake", serial_module=fake_serial) as p:
        assert FakeSerial.log[0] == ("dtr", False) or FakeSerial.log[1] == ("rts", False)
        assert FakeSerial.log.index(("open", False, False)) < len(FakeSerial.log)
        p.key()
        assert p._s._dtr is True and p._s._rts is False and p.keyed
    assert p._s._dtr is False and not p._s.is_open and not p.keyed


def test_ptt_rts_and_invert(fake_serial):
    p = devices.Ptt("/dev/fake", "rts", serial_module=fake_serial)
    p.key()
    assert p._s._rts is True and p._s._dtr is False
    p.release()
    q = devices.Ptt("/dev/fake", "dtr", invert=True, serial_module=fake_serial)
    assert q._s._dtr is True        # idle is asserted when active low
    q.key()
    assert q._s._dtr is False
    q.release()


def test_ptt_released_on_exception_and_signal(fake_serial):
    p = devices.Ptt("/dev/fake", serial_module=fake_serial)
    with pytest.raises(RuntimeError):
        with p:
            p.key()
            raise RuntimeError("boom")
    assert p._s._dtr is False and not p._s.is_open
    p = devices.Ptt("/dev/fake", serial_module=fake_serial)
    with pytest.raises(KeyboardInterrupt):
        with p:
            p.key()
            signal.raise_signal(signal.SIGINT)
    assert p._s._dtr is False and not p._s.is_open


class FakeSd:
    def __init__(self, ptt=None):
        self.ptt, self.events = ptt, []

    def query_devices(self):
        return [{"name": "HDA Intel", "max_input_channels": 2, "max_output_channels": 2},
                {"name": "All-In-One-Cable: USB Audio", "max_input_channels": 1, "max_output_channels": 1}]

    def play(self, x, fs, device=None):
        self.events.append(("play", len(x), fs, device, self.ptt._s._dtr if self.ptt else None))

    def wait(self):
        pass

    def rec(self, n, samplerate, channels, device, dtype):
        return np.full((n, channels), 0.1, dtype="float32")


def test_find_device_by_substring():
    assert devices.find_device("all-in-one", "output", FakeSd()) == 1
    assert devices.find_device(None, "input", FakeSd()) is None
    with pytest.raises(SystemExit):
        devices.find_device("nothing", "input", FakeSd())


def test_play_keys_around_audio_and_releases_on_failure(fake_serial, monkeypatch):
    monkeypatch.setattr(devices.time, "sleep", lambda s: None)
    p = devices.Ptt("/dev/fake", serial_module=fake_serial)
    sd = FakeSd(p)
    devices.play_audio(np.zeros(480), FS, 1, ptt=p, sd=sd)
    assert sd.events == [("play", 480, FS, 1, True)] and p._s._dtr is False

    class Boom(FakeSd):
        def play(self, *a, **k):
            raise OSError("device gone")

    p.key()
    with pytest.raises(OSError):
        devices.play_audio(np.zeros(10), FS, 1, ptt=p, sd=Boom())
    assert p._s._dtr is False
    p.release()


def test_level_stats_and_record():
    x = 0.1 + 0.5 * np.sin(np.arange(4800) * 0.3)
    s = devices.level_stats(x)
    assert abs(s["dc"] - 0.1) < 0.01 and abs(s["peak_dbfs"] - 20 * np.log10(0.6)) < 0.3
    assert "CLIPPED" in devices.format_levels(devices.level_stats(np.ones(10)))
    rec = devices.record_audio(0.5, FS, None, 1, sd=FakeSd())
    assert rec.shape == (FS // 2, 1) and abs(devices.level_stats(rec[:, 0])["dc"] - 0.1) < 1e-6


def test_ptt_test_command_refuses_long_keying(fake_serial, capsys):
    assert cli.main(["ptt-test", "--ptt-port", "/dev/fake", "--seconds", "9"]) == 2
    assert cli.main(["tx", "--list-plans"]) == 0


def test_clean_mcs0_frame_reports_channel_snr_and_picks_mcs6():
    # ADR 0006: the designed clipping no longer caps an MCS 0 frame's SNR at ~22 dB.
    x, m = tx.synthesize([tx.FrameSpec(0, 24, blocks=2)], tone_s=1.0, callsign="N0CALL", lead_s=0.5)
    rep = run(x, m)
    f = rep["frames"][0]
    assert f["snr7_db"] >= 40.0 and not f["snr_fallback"]
    assert f["adaptation"]["first_frame"] == 6
    assert f["snr_known_symbols"] == f["snr_total_symbols"]
    assert len(f["snr_per_tone_db"]) == 24 and min(f["snr_per_tone_db"]) >= 40.0
    assert rep["summary"]["snr_fallback_frames"] == 0


def test_report_marks_fallback_frames():
    x, m = tx.synthesize([tx.FrameSpec(0, 24, blocks=2)], tone_s=1.0, callsign="N0CALL", lead_s=0.5)
    rep = run(x, m)
    rep["frames"][0].update(snr_fallback=True)
    rep["summary"]["snr_fallback_frames"] = 1
    rec = {"duration_s": 1.0, "sample_rate_in": FS, "channels": 1, "channel_used": "mono",
           "peak_dbfs": -3.0, "rms_dbfs": -20.0}
    text = rx.format_report({"recording": rec, **rep})
    assert "*" in text and "fallback" in text
