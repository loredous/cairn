"""Generate receiver fixtures for the Rust port from the Stage 1 simulator.

    uv run --package cairn-sim python modem/tools/gen_rx_fixtures.py [--out DIR]
    uv run --package cairn-sim python modem/tools/gen_rx_fixtures.py --sweep N --out DIR

Default mode writes ``modem/fixtures/``:

``rx/<case>.wav`` + ``rx/<case>.json``
    A frame built by ``frame.build``, passed through a channel (none, AWGN,
    the SPEC 14.2 ``FmChannel`` profiles, a sample-clock offset, a noise
    burst), as 48 kHz 16-bit mono audio. The JSON holds the frame
    configuration, the header message and payloads, what the Python
    detector found and what the Python receiver decoded from exactly the
    samples in the WAV file (``tests/rx_fixtures.rs`` checks the Rust
    receiver against it). Cases named ``hdr_*`` carry real header fields and
    control blocks (``fields``) and are decoded header first: the receivers
    know only the width and mask (``frame.receive(rx, link=...)``).
``tx/<case>.f64`` + ``tx/<case>.json``
    Frames after clip-and-filter (little-endian float64), with their inputs,
    to check the Rust clip-and-filter against the simulator's.

``--sweep N`` instead writes N AWGN frames per SNR near the MCS 2 threshold
(not committed; ``examples/rx_agreement.rs`` compares the receivers block
by block on them).

The output is deterministic (fixed seeds).
"""

from __future__ import annotations

import argparse
import json
import wave
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np

from cairn_sim import channel, fields, frame, params, preamble

ROOT = Path(__file__).resolve().parents[1]
GUARD = 400
PEAK = 0.5          # WAV peak level (-6 dBFS) before 16-bit rounding


@dataclass(frozen=True)
class Case:
    name: str
    width: int
    mcs: int
    n_blocks: int
    chan: str = "none"            # none, awgn, or an FmChannel profile
    snr_db: float | None = None
    cp: int = params.CP_SHORT
    usable: tuple[int, ...] | None = None
    sco_ppm: float = 0.0
    burst_db: float | None = None  # 10 ms burst this far above the signal power
    seed: int = 1
    extra: dict = field(default_factory=dict)
    header: fields.Header | None = None   # real header (decoded header first); None: random bits
    controls: tuple = ()


def _all_but(*tones: int) -> tuple[int, ...]:
    return tuple(k for k in range(params.MAX_TONES) if k not in tones)


CASES: tuple[Case, ...] = (
    Case("clean_w24_mcs0", 24, 0, 2, seed=11),
    Case("clean_w56_mcs6", 56, 6, 6, seed=12),
    Case("clean_w24_mcs6_long", 24, 6, 4, cp=params.CP_LONG, seed=13),
    Case("awgn_w24_mcs2_snr7", 24, 2, 8, "awgn", 7.0, seed=21),
    Case("awgn_w24_mcs2_snr8", 24, 2, 8, "awgn", 8.0, seed=22),
    Case("awgn_w24_mcs2_snr10", 24, 2, 8, "awgn", 10.0, seed=23),
    Case("awgn_w56_mcs4_snr18", 56, 4, 8, "awgn", 18.0, seed=24),
    Case("awgn_w24_mcs5_snr23", 24, 5, 8, "awgn", 23.0, seed=25),
    Case("mic_w24_mcs3_snr18", 24, 3, 8, "mic", 18.0, seed=31),
    Case("repeater_w24_mcs2_long_snr16", 24, 2, 8, "repeater", 16.0, cp=params.CP_LONG, seed=32),
    Case("repeater_w24_mcs4_long_masked_snr24", 24, 4, 8, "repeater", 24.0, cp=params.CP_LONG,
         usable=_all_but(0, 1, 2), seed=33),
    Case("masked_w56_mcs4_long_snr22", 56, 4, 8, "awgn", 22.0, cp=params.CP_LONG,
         usable=_all_but(0, 1, 2, 37, 45, 46), seed=34),
    Case("masked_w30_mcs1_flat12_snr12", 30, 1, 4, "flat12", 12.0, usable=_all_but(9, 25, 40), seed=35),
    Case("sco_p100_w24_mcs3_snr20", 24, 3, 12, "ideal", 20.0, sco_ppm=100.0, seed=41),
    Case("sco_m100_w56_mcs2_snr18", 56, 2, 12, "ideal", 18.0, sco_ppm=-100.0, seed=42),
    Case("burst_w24_mcs2_snr12", 24, 2, 8, "awgn", 12.0, burst_db=20.0, seed=51),
)

_S1, _S2 = fields.Address("N0CALL", 1), fields.Address("N0CALL", 2)
_ACK = fields.AckBlock(base_sequence=40, bitmap=0xFF, requested_mcs=2, snr_q=fields.snr_code(15.5))
_T = fields.FrameType
HEADER_CASES: tuple[Case, ...] = (
    Case("hdr_mic_w24_mcs2_ctrl_snr16", 24, 2, 4, "mic", 16.0, seed=71,
         header=fields.Header(_T.DATA, _S1, _S2, sequence=48, mcs=2, n_control=1, n_data=4, session=True),
         controls=(_ACK,)),
    Case("hdr_awgn_w30_mcs3_probe_ctrl2_masked_snr18", 30, 3, 3, "awgn", 18.0, usable=_all_but(1, 9, 25),
         seed=72, header=fields.Header(_T.DATA, _S2, _S1, sequence=7, mcs=3, n_control=2, n_data=3,
                                       session=True, probe=True),
         controls=(_ACK, fields.IdBlock("N0CALL", 2, "FN31"))),
    Case("hdr_repeater_w24_ack_long_snr14", 24, 0, 0, "repeater", 14.0, cp=params.CP_LONG, seed=73,
         header=fields.Header(_T.ACK, _S2, _S1, n_control=1, session=True), controls=(_ACK,)),
    Case("hdr_awgn_w24_probe_only_snr10", 24, 0, 0, "awgn", 10.0, seed=74,
         header=fields.Header(_T.PROBE, _S1, _S2)),
)

TX_CASES: tuple[Case, ...] = (
    Case("tx_w24_mcs6_long", 24, 6, 2, cp=params.CP_LONG, seed=61),
    Case("tx_w56_mcs4_long_masked", 56, 4, 2, cp=params.CP_LONG, usable=_all_but(0, 1, 2, 37, 45, 46), seed=62),
    Case("tx_w30_mcs1_masked", 30, 1, 1, usable=_all_but(9, 25, 40), seed=63),
)


def link(case: Case) -> frame.LinkState:
    return frame.LinkState(case.width, None if case.usable is None else frozenset(case.usable), case.cp)


def config(case: Case) -> frame.FrameConfig:
    mask = None if case.usable is None else frozenset(case.usable)
    if case.header is not None:
        return frame.FrameConfig.from_header(case.header, link(case))
    return frame.FrameConfig(width=case.width, mcs=case.mcs, n_blocks=case.n_blocks, cp=case.cp, mask=mask)


def build(case: Case, cfg: frame.FrameConfig, rng: np.random.Generator) -> frame.TxFrame:
    if case.header is not None:
        return frame.build(cfg, rng, header=case.header, controls=list(case.controls))
    return frame.build(cfg, rng)


def bits_hex(bits: np.ndarray) -> str:
    return np.packbits(np.asarray(bits, dtype=np.uint8)).tobytes().hex()


def through_channel(case: Case, x: np.ndarray, rng: np.random.Generator) -> np.ndarray:
    pad = np.zeros(GUARD)
    x = np.concatenate([pad, x, pad])
    if case.chan == "none":
        y = x
    elif case.chan == "awgn":
        y = channel.awgn(x, case.snr_db, case.width, rng)
    else:
        y = channel.FmChannel(case.chan, snr_db=case.snr_db, sco_ppm=case.sco_ppm, width=case.width)(x, rng)
    if case.burst_db is not None:
        cfg = config(case)
        p = float(np.mean(x[GUARD:-GUARD] ** 2))
        at = GUARD + (cfg.total_symbols - 10) * cfg.stride
        y = y.copy()
        y[at:at + 480] += rng.standard_normal(480) * np.sqrt(p * 10 ** (case.burst_db / 10))
    return y


def to_int16(y: np.ndarray) -> tuple[np.ndarray, float]:
    scale = PEAK / float(np.max(np.abs(y)))
    q = np.clip(np.rint(y * scale * 32768), -32768, 32767).astype(np.int16)
    return q, scale


def write_wav(path: Path, q: np.ndarray) -> None:
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(params.FS)
        w.writeframes(q.astype("<i2").tobytes())


def python_receive(case: Case, cfg: frame.FrameConfig, rx: np.ndarray, tx: frame.TxFrame) -> dict:
    dets = preamble.detect(rx)
    near = sorted((d for d in dets if d.cp == cfg.cp and abs(d.start - GUARD) < cfg.stride),
                  key=lambda d: -d.quality)
    start = near[0].start if near else None
    out = {"detections": [{"start": int(d.start), "cp": int(d.cp), "quality": d.quality,
                           "autocorr": d.autocorr, "ppm": d.ppm, "coarse_start": d.coarse_start}
                          for d in dets]}
    if start is None:
        out["rx"] = None
        return out
    if case.header is not None:
        r = frame.receive(rx, link=frame.LinkState(cfg.width, cfg.mask, cfg.cp), start=start)
    else:
        r = frame.receive(rx, cfg, start=start)
    correct = r.block_ok & np.all(r.block_bits == tx.block_bits, axis=1)
    out["rx"] = {"start": int(start), "header_ok": bool(r.header_ok),
                 "header_correct": bool(r.header_ok and np.array_equal(r.header_bits, tx.header_bits)),
                 "block_ok": [bool(v) for v in r.block_ok],
                 "block_correct": [bool(v) for v in correct],
                 "iterations": [int(v) for v in r.iterations],
                 "snr_db": [float(v) for v in r.snr_db]}
    if case.header is not None:
        out["rx"]["header_fields_ok"] = r.header == case.header
        out["rx"]["control_ok"] = [bool(v) for v in r.control_ok]
        out["rx"]["control_correct"] = [bool(v) for v in r.control_ok & np.all(r.control_bits == tx.control_bits,
                                                                                axis=1)]
    return out


def case_doc(case: Case, cfg: frame.FrameConfig, tx: frame.TxFrame) -> dict:
    return {"name": case.name, "width": case.width, "mcs": case.mcs, "n_blocks": case.n_blocks,
            "cp": case.cp, "usable_tones": None if case.usable is None else list(case.usable),
            "channel": case.chan, "snr_db": case.snr_db, "sco_ppm": case.sco_ppm,
            "burst_db": case.burst_db, "seed": case.seed, "guard": GUARD,
            "header_message_hex": bits_hex(tx.header_bits[:-16]),
            "payload_hex": [bits_hex(b[:-16]) for b in tx.block_bits],
            "total_symbols": cfg.total_symbols,
            **({} if case.header is None else {
                "header_first": True, "n_control": cfg.n_control, "probe": cfg.has_probe,
                "control_message_hex": [bits_hex(b[:-16]) for b in tx.control_bits]})}


def rx_case(case: Case, out_dir: Path) -> dict:
    rng = np.random.default_rng(case.seed)
    cfg = config(case)
    tx = build(case, cfg, rng)
    y = through_channel(case, tx.samples, rng)
    q, scale = to_int16(y)
    rx = q.astype(float) / 32768
    write_wav(out_dir / f"{case.name}.wav", q)
    doc = case_doc(case, cfg, tx)
    doc["wav_scale"] = scale
    doc["python"] = python_receive(case, cfg, rx, tx)
    (out_dir / f"{case.name}.json").write_text(json.dumps(doc, indent=1) + "\n")
    return doc


def tx_case(case: Case, out_dir: Path) -> None:
    rng = np.random.default_rng(case.seed)
    cfg = config(case)
    tx = frame.build(cfg, rng)
    tx.samples.astype("<f8").tofile(out_dir / f"{case.name}.f64")
    doc = case_doc(case, cfg, tx)
    doc["clip_db"] = cfg.clip_db
    doc["n_samples"] = int(tx.samples.size)
    (out_dir / f"{case.name}.json").write_text(json.dumps(doc, indent=1) + "\n")


def sweep(n: int, out_dir: Path, mcs: int = 2, snrs: tuple[float, ...] = (6.5, 7.0, 7.5, 8.0)) -> None:
    for snr in snrs:
        for i in range(n):
            case = Case(f"sweep_mcs{mcs}_snr{snr:g}_{i:03d}", 24, mcs, 16, "awgn", snr,
                        seed=10_000 + int(snr * 10) * 1000 + i)
            doc = rx_case(case, out_dir)
            rx = doc["python"]["rx"]
            ok = sum(rx["block_correct"]) if rx and rx["header_ok"] else 0
            print(case.name, ok, flush=True)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--out", type=Path, default=ROOT / "fixtures")
    ap.add_argument("--sweep", type=int, default=0, help="frames per SNR for the agreement sweep")
    ap.add_argument("--mcs", type=int, default=2)
    ap.add_argument("--snrs", type=str, default="6.5,7,7.5,8")
    args = ap.parse_args()
    if args.sweep:
        args.out.mkdir(parents=True, exist_ok=True)
        sweep(args.sweep, args.out, args.mcs, tuple(float(s) for s in args.snrs.split(",")))
        return
    (args.out / "rx").mkdir(parents=True, exist_ok=True)
    (args.out / "tx").mkdir(parents=True, exist_ok=True)
    for case in CASES + HEADER_CASES:
        doc = rx_case(case, args.out / "rx")
        rx = doc["python"]["rx"]
        summary = "missed" if rx is None else (
            f"header {'ok' if rx['header_ok'] else 'FAIL'}, blocks {sum(rx['block_correct'])}/{case.n_blocks}")
        if rx is not None and "control_ok" in rx:
            summary += f", fields {'ok' if rx['header_fields_ok'] else 'FAIL'}, controls {rx['control_correct']}"
        print(f"{case.name}: {summary}")
    for case in TX_CASES:
        tx_case(case, args.out / "tx")
        print(case.name)


if __name__ == "__main__":
    main()
