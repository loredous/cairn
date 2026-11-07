"""``cairn-bench tx``: a WAV of test frames and the manifest that scores them."""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass

import numpy as np

from .. import frame, params
from ..link_sim import Budget, default_frame_s
from . import audio

FS = params.FS
FORMAT = "cairn-bench/1"
LEVEL_TONE_HZ = 1000.0          # SPEC 7.1
CW_TONE_HZ = 800.0
ID_INTERVAL_S = 540.0           # Part 97 asks for at least every 10 minutes; leave a minute of margin
MAX_KEYED_WARN_S = 120.0        # many radios time out between 1 and 3 minutes of transmit


@dataclass(frozen=True)
class FrameSpec:
    mcs: int
    width: int = params.BASE_TONES
    prefix: str = "short"                       # "short" (60 samples) or "long" (120)
    silenced: tuple[int, ...] = ()              # tones left silent in data symbols (SPEC 5.4)
    frame_s: float | None = None                # target frame time; default per rung (SPEC 10.2)
    blocks: int | None = None                   # override the block count

    def __post_init__(self) -> None:
        if self.prefix not in ("short", "long"):
            raise ValueError("prefix must be 'short' or 'long'")

    @property
    def cp(self) -> int:
        return params.CP_SHORT if self.prefix == "short" else params.CP_LONG

    def config(self, n_blocks: int) -> frame.FrameConfig:
        mask = None
        if self.silenced:
            mask = frozenset(range(self.width)) - frozenset(self.silenced)
        return frame.FrameConfig(width=self.width, mcs=self.mcs, n_blocks=n_blocks, cp=self.cp, mask=mask)

    def n_blocks(self) -> int:
        if self.blocks is not None:
            return max(1, self.blocks)
        mask = self.config(1).mask
        b = Budget(self.mcs, self.width, self.frame_s or default_frame_s(self.mcs), cp=self.cp, mask=mask)
        return max(1, b.blocks)


# --- plans -------------------------------------------------------------------

PLANS = {
    "quick": "MCS 0, 2, 4 at 24 tones (about 12 s)",
    "ladder24": "every rung, MCS 0-6, at 24 tones",
    "width": "MCS 2 at 24, 32, 40, 48 and 56 tones",
    "prefix": "MCS 0 and MCS 2 at 24 tones, short then long prefix",
    "mask": "MCS 2 at 40 tones, then the same with tones 8-11 and 30-33 silenced",
    "custom": "the cross product of --rungs, --widths and --prefix",
}


def parse_tones(text: str) -> tuple[int, ...]:
    """``"8-11,30"`` -> (8, 9, 10, 11, 30)."""
    out: list[int] = []
    for part in filter(None, (p.strip() for p in text.split(","))):
        lo, _, hi = part.partition("-")
        out += range(int(lo), int(hi or lo) + 1)
    return tuple(sorted(set(out)))


def parse_ints(text: str) -> tuple[int, ...]:
    return parse_tones(text)


def make_plan(name: str, *, rungs=(0, 2, 4), widths=(24,), prefix="short", silenced=(),
              frame_s=None, blocks=None) -> list[FrameSpec]:
    kw = dict(frame_s=frame_s, blocks=blocks)
    if name == "quick":
        return [FrameSpec(m, 24, **kw) for m in (0, 2, 4)]
    if name == "ladder24":
        return [FrameSpec(m, 24, **kw) for m in range(len(params.RUNGS))]
    if name == "width":
        return [FrameSpec(2, w, **kw) for w in (24, 32, 40, 48, 56)]
    if name == "prefix":
        return [FrameSpec(m, 24, p, **kw) for m in (0, 2) for p in ("short", "long")]
    if name == "mask":
        return [FrameSpec(2, 40, **kw), FrameSpec(2, 40, silenced=(8, 9, 10, 11, 30, 31, 32, 33), **kw)]
    if name == "custom":
        prefixes = ("short", "long") if prefix == "both" else (prefix,)
        return [FrameSpec(m, w, p, tuple(s for s in silenced if s < w), **kw)
                for m in rungs for w in widths for p in prefixes]
    raise ValueError(f"unknown plan {name!r}; choose from {sorted(PLANS)}")


# --- synthesis ----------------------------------------------------------------


def peak_amplitude(peak_dbfs: float) -> float:
    return 10 ** (peak_dbfs / 20)


def level_tone(duration_s: float, peak: float, fade_s: float = 0.02) -> np.ndarray:
    """SPEC 7.1 level-setting tone: 1000 Hz at the data's peak amplitude."""
    n = int(round(duration_s * FS))
    x = peak * np.sin(2 * np.pi * LEVEL_TONE_HZ * np.arange(n) / FS)
    r = min(int(fade_s * FS), n // 2)
    if r:
        ramp = 0.5 - 0.5 * np.cos(np.pi * (np.arange(r) + 0.5) / r)
        x[:r] *= ramp
        x[n - r:] *= ramp[::-1]
    return x


def synthesize(specs: list[FrameSpec], *, repeat: int = 1, seed: int = 1, lead_s: float = 1.0,
               tone_s: float = 5.0, gap_s: float = 0.5, callsign: str | None = None,
               wpm: float = 20.0, id_interval_s: float = ID_INTERVAL_S,
               peak_dbfs: float = params.PEAK_DBFS) -> tuple[np.ndarray, dict]:
    """Assemble the test signal and its manifest. Frames from ``frame.build`` are
    unit RMS; each is scaled so its own peak sits at ``peak_dbfs`` (SPEC 7.1)."""
    if not specs:
        raise ValueError("the plan has no frames")
    if peak_dbfs > 0:
        raise ValueError("peak level must be at or below 0 dBFS")
    peak = peak_amplitude(peak_dbfs)
    chunks: list[np.ndarray] = []
    pos = 0

    def put(x: np.ndarray) -> int:
        nonlocal pos
        chunks.append(x)
        at = pos
        pos += len(x)
        return at

    def silence(s: float) -> None:
        put(np.zeros(int(round(s * FS))))

    cw_ids: list[dict] = []
    last_id = [None]

    def put_id() -> None:
        x = audio.morse(callsign, wpm, CW_TONE_HZ, peak, FS)
        at = put(x)
        cw_ids.append({"start_s": at / FS, "duration_s": len(x) / FS, "text": callsign.upper(),
                       "wpm": wpm, "freq_hz": CW_TONE_HZ})
        last_id[0] = at / FS

    silence(lead_s)
    if callsign:
        put_id()
        silence(gap_s)
    tone_info = None
    if tone_s > 0:
        at = put(level_tone(tone_s, peak))
        tone_info = {"start_s": at / FS, "duration_s": tone_s, "freq_hz": LEVEL_TONE_HZ,
                     "peak_dbfs": peak_dbfs}
        silence(gap_s)

    # Build every frame first, so the ID check can look one item ahead.
    items = []
    index = 0
    for rep in range(repeat):
        for spec in specs:
            n_blocks = spec.n_blocks()
            cfg = spec.config(n_blocks)
            fseed = seed * 1000 + index
            built = frame.build(cfg, np.random.default_rng(fseed))
            x = built.samples * (peak / float(np.abs(built.samples).max()))
            items.append((index, spec, cfg, n_blocks, fseed, x))
            index += 1
    id_len = len(audio.morse(callsign, wpm, CW_TONE_HZ, peak, FS)) / FS if callsign else 0.0

    def id_due(next_len_s: float) -> bool:
        """Would the next signal end more than ``id_interval_s`` after the last ID started?"""
        return bool(callsign) and pos / FS + next_len_s - last_id[0] > id_interval_s

    frames = []
    for k, (index, spec, cfg, n_blocks, fseed, x) in enumerate(items):
        at = put(x)
        silence(gap_s)
        frames.append({
            "index": index, "mcs": spec.mcs, "width": spec.width, "prefix": spec.prefix,
            "cp": cfg.cp, "silenced": list(spec.silenced), "n_blocks": n_blocks, "seed": fseed,
            "start_s": at / FS, "start_sample": at, "n_samples": len(x), "duration_s": len(x) / FS,
            "clip_db": cfg.clip_db,
            "peak_dbfs": audio.db(float(np.abs(x).max())),
            "rms_dbfs": audio.db(float(np.sqrt(np.mean(x ** 2)))),
        })
        nxt = len(items[k + 1][5]) / FS if k + 1 < len(items) else id_len
        if id_due(nxt + gap_s) or (callsign and k + 1 < len(items) and id_due(nxt + gap_s + id_len + gap_s)):
            put_id()
            silence(gap_s)
    if callsign and (not cw_ids or cw_ids[-1]["start_s"] < frames[-1]["start_s"]):
        put_id()
    silence(0.5)
    x = np.concatenate(chunks)
    manifest = {
        "format": FORMAT, "fs": FS, "peak_dbfs": peak_dbfs, "seed": seed, "lead_s": lead_s,
        "gap_s": gap_s, "callsign": callsign.upper() if callsign else None,
        "n_samples": len(x), "duration_s": len(x) / FS, "level_tone": tone_info, "cw_ids": cw_ids,
        "frames": frames,
    }
    return x, manifest


def write(path: str, manifest_path: str, x: np.ndarray, manifest: dict) -> None:
    audio.write_wav(path, x, FS)
    with open(manifest_path, "w") as f:
        json.dump(manifest, f, indent=1)


def describe(manifest: dict) -> str:
    lines = [f"{manifest['duration_s']:.1f} s, {len(manifest['frames'])} frames, "
             f"{manifest['n_samples']} samples at {manifest['fs']} Hz"]
    tone = manifest["level_tone"]
    if tone:
        lines.append(f"  {tone['start_s']:7.2f} s  level tone {tone['freq_hz']:.0f} Hz, "
                     f"{tone['duration_s']:.1f} s, peak {tone['peak_dbfs']:.1f} dBFS")
    events = [(c["start_s"], f"CW ID {c['text']} ({c['duration_s']:.1f} s)") for c in manifest["cw_ids"]]
    for f in manifest["frames"]:
        extra = f", {len(f['silenced'])} tones silenced" if f["silenced"] else ""
        events.append((f["start_s"], f"frame {f['index']:2d}: MCS {f['mcs']} x {f['width']} tones, "
                       f"{f['prefix']} prefix, {f['n_blocks']} blocks, {f['duration_s']:.2f} s{extra}"))
    for t, text in sorted(events):
        lines.append(f"  {t:7.2f} s  {text}")
    if manifest["duration_s"] > MAX_KEYED_WARN_S:
        lines.append(f"  note: {manifest['duration_s']:.0f} s is longer than many radios' transmit "
                     f"time-out; play in parts or raise the time-out timer")
    return "\n".join(lines)
