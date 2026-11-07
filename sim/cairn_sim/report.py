"""Generate sim/report.md and its plots from sim/results/*.json.

    uv run --package cairn-sim python -m cairn_sim.report

Sections whose results file is missing are reported as not yet run. The
narrative findings live in sim/findings.md and are included verbatim.
"""

from __future__ import annotations

import json
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import numpy as np  # noqa: E402

from . import frame, mapping, ofdm, params  # noqa: E402
from .link_sim import Budget, default_frame_s  # noqa: E402

SIM = Path(__file__).resolve().parents[1]
RESULTS = SIM / "results"
PLOTS = RESULTS / "plots"


def _load(name: str) -> list[dict] | None:
    path = RESULTS / f"{name}.json"
    return json.loads(path.read_text()) if path.exists() else None


def _fmt(x: float | None, nd: int = 1) -> str:
    return "—" if x is None else f"{x:.{nd}f}"


def _floor(row: dict) -> bool:
    """True when 1% is reached only far beyond where the waterfall ended
    (BLER first under 5%), or never: an error floor rather than a threshold."""
    pts = sorted(row["points"], key=lambda p: p["snr_db"])
    knee = next((p["snr_db"] for p in pts if p["bler"] < 0.05), None)
    if row["threshold_db"] is None:          # never bracketed: a floor only if 1% was never reached
        return min(p["bler"] for p in pts) >= 0.01
    return knee is not None and row["threshold_db"] - knee > 4.0


def _clip(row: dict) -> float:
    """Clip level a row ran at; rows from before per-rung clipping ran at 7 dB."""
    return row.get("clip_db", 7.0)


def _thresholds(rows: list[dict], **match) -> dict[int, float | None]:
    """1% thresholds as 7 dB-equivalent SNR (SPEC 6.2, ADR 0003): the
    average-power threshold plus the row's clip level minus 7 dB."""
    out = {}
    for r in rows:
        if all(r.get(k) == v for k, v in match.items()):
            out[r["mcs"]] = None if _floor(r) else r["threshold_db"] + _clip(r) - 7.0
    return out


def _round_up_half(x: float) -> float:
    return float(np.ceil(x * 2 - 1e-9) / 2)


def _floors(rows: list[dict], **match) -> dict[int, float]:
    """Lowest BLER reached by floor-limited rows, keyed by MCS."""
    return {r["mcs"]: min(p["bler"] for p in r["points"]) for r in rows
            if all(r.get(k) == v for k, v in match.items()) and _floor(r)}


def bler_plot(rows: list[dict], title: str, fname: str, **match) -> str:
    PLOTS.mkdir(parents=True, exist_ok=True)
    fig, ax = plt.subplots(figsize=(7, 4.2))
    for r in sorted((r for r in rows if all(r.get(k) == v for k, v in match.items())),
                    key=lambda r: r["mcs"]):
        pts = sorted(r["points"], key=lambda p: p["snr_db"])
        x = [p["snr_db"] + _clip(r) - 7.0 for p in pts]
        y = [max(p["bler"], 0.5 / max(p["blocks"], 1)) for p in pts]
        ax.semilogy(x, y, "o-", label=f"MCS {r['mcs']} ({params.RUNGS[r['mcs']].modulation} "
                    f"{params.RUNGS[r['mcs']].code_rate})")
        ax.axvline(params.RUNGS[r["mcs"]].nominal_snr_db, color=ax.lines[-1].get_color(),
                   ls=":", lw=0.8)
    ax.axhline(0.01, color="k", lw=0.6)
    ax.set_xlabel("7 dB-equivalent audio SNR (dB)")
    ax.set_ylabel("block error rate")
    ax.set_title(title + " (dotted: SPEC 6.2 nominal)")
    ax.set_ylim(1e-4, 1.05)
    ax.grid(True, which="both", alpha=0.3)
    ax.legend(fontsize=7, loc="lower left")
    fig.tight_layout()
    fig.savefig(PLOTS / fname, dpi=110)
    plt.close(fig)
    return f"results/plots/{fname}"


def papr_section() -> list[str]:
    rng = np.random.default_rng(14)
    lines = ["## PAPR before and after clip-and-filter (SPEC 7.1)", ""]
    PLOTS.mkdir(parents=True, exist_ok=True)
    fig, ax = plt.subplots(figsize=(6, 3.6))
    lines += ["| Width | Before: median / 99.9% / max (dB) | After: median / max (dB) |",
              "| --- | --- | --- |"]
    for width in (24, 56):
        n = 10000
        tones = np.array(params.data_tones_for(width))
        grid = np.zeros((n, width), dtype=complex)
        grid[:, tones] = mapping.modulate(rng.integers(0, 2, n * len(tones) * 4, dtype=np.uint8),
                                          4).reshape(n, len(tones))
        x = ofdm.modulate(grid, active=width)
        before = ofdm.papr_db(x)
        after = ofdm.papr_db(ofdm.clip_and_filter(x, width, tones, scale_tones=width))
        lines.append(f"| {width} | {np.median(before):.1f} / {np.percentile(before, 99.9):.1f} / "
                     f"{before.max():.1f} | {np.median(after):.1f} / {after.max():.2f} |")
        for data, ls in ((before, "-"), (after, "--")):
            s = np.sort(data)
            ax.semilogy(s, 1 - np.arange(s.size) / s.size, ls,
                        label=f"{width} tones, {'before' if ls == '-' else 'after'}")
    ax.set_xlabel("PAPR (dB)")
    ax.set_ylabel("P(PAPR > x)")
    ax.grid(True, which="both", alpha=0.3)
    ax.legend(fontsize=7)
    fig.tight_layout()
    fig.savefig(PLOTS / "papr.png", dpi=110)
    plt.close(fig)
    lines += ["", "10,000 random 16-QAM symbols per width.", "", "![PAPR CCDF](results/plots/papr.png)", ""]
    # Distortion the clipper leaves in band.
    lines += ["### In-band distortion left by clipping", "",
              "| Clip level (dB above RMS) | Signal-to-distortion (dB), 24 tones |", "| --- | --- |"]
    for clip_db in (6.0, 7.0, 8.0, 9.0, 10.0):
        cfg = frame.FrameConfig(mcs=3, n_blocks=40, clip_db=clip_db)
        tx = frame.build(cfg, rng)
        t = ofdm.demodulate(tx.samples, cfg.cp, cfg.total_symbols, 24, cfg.cp, active=24)
        m = tx.grid != 0
        m[:2] = False   # S1 symbols are constant-amplitude and barely clipped
        sdr = -10 * np.log10(np.mean(np.abs(t[m] - tx.grid[m]) ** 2) / np.mean(np.abs(tx.grid[m]) ** 2))
        lines.append(f"| {clip_db:.0f} | {sdr:.1f} |")
    lines.append("")
    return lines


def budget_section() -> list[str]:
    lines = ["## Time budget and net throughput (SPEC 7.4, Appendix A.2)", "",
             "Frames of 2 s at MCS 0–1 and 4 s otherwise, TXDelay 150 ms, peer ACK cycle "
             "0.51 s. The cycle uses the frame's actual length (Appendix A assumes a full frame).", "",
             "| MCS | Blocks 24 / 56 | Symbols per block 24 / 56 | Cycle (s) | Net kbps 24 / 56 | A.2 net kbps |",
             "| --- | --- | --- | --- | --- | --- |"]
    a2 = {0: "1.2 / 2.9", 1: "1.6 / 4.5", 2: "3.2 / 7.5", 3: "4.8 / 11.3", 4: "6.3 / 15.1",
          5: "7.9 / 19.0", 6: "9.5 / 22.6"}
    for m in range(7):
        a, c = Budget(m, 24, default_frame_s(m)), Budget(m, 56, default_frame_s(m))
        lines.append(f"| {m} | {a.blocks} / {c.blocks} | {a.symbols_per_block:.1f} / {c.symbols_per_block:.1f} "
                     f"| {a.cycle_s:.2f} | {a.net_bps / 1e3:.2f} / {c.net_bps / 1e3:.2f} | {a2[m]} |")
    b = Budget(3)
    lines += ["", f"SPEC 7.4 worked example (24 tones, MCS 3, 4 s): {b.blocks} blocks, cycle "
              f"{b.cycle_s:.3f} s, {b.net_bps / 1e3:.2f} kbps net (spec: 21 blocks, 4.46 s, 4.75 kbps).", "",
              "### Net throughput against TXDelay and frame length (24 tones, MCS 3)", "",
              "| TXDelay \\ frame | 1 s | 2 s | 3 s | 4 s |", "| --- | --- | --- | --- | --- |"]
    for txd in (0.05, 0.15, 0.3, 0.5):
        cells = [f"{Budget(3, frame_s=f, txdelay_s=txd).net_bps / 1e3:.2f}" for f in (1.0, 2.0, 3.0, 4.0)]
        lines.append(f"| {int(txd * 1000)} ms | " + " | ".join(cells) + " |")
    lines.append("")
    return lines


def thresholds_section() -> list[str]:
    lines = ["## Rung thresholds (1% block error)", ""]
    awgn = _load("awgn")
    chans = _load("channels")
    if awgn is None:
        return lines + ["AWGN sweep not yet run.", ""]
    cols = [("AWGN 24", awgn, {"width": 24}), ("AWGN 56", awgn, {"width": 56})]
    if chans:
        for prof, w in (("mic", 24), ("flat12", 30), ("flat25", 56), ("repeater", 24)):
            if any(r["profile"] == prof for r in chans):
                cols.append((f"{prof} ({w})", chans, {"profile": prof}))
    masked = _load("repeater_masked")
    if masked:
        cols.append(("repeater, tones 0–2 masked (24)", masked, {}))
    head = ("| MCS | Rung | Clip (dB) | SPEC 6.2 nominal | " + " | ".join(c for c, _, _ in cols)
            + " | Worst Δ vs nominal | Table value |")
    lines += [head, "|" + " --- |" * (6 + len(cols))]
    for r in params.RUNGS:
        cells, deltas = [], []
        for _, rows, match in cols:
            v = _thresholds(rows, **match).get(r.mcs)
            fl = _floors(rows, **match).get(r.mcs)
            if v is not None:
                cells.append(f"{v:.1f}")
                deltas.append(v - r.nominal_snr_db)
            elif fl is not None:
                cells.append(f"floor {100 * fl:.1f}%" if fl < 0.5 else "fails")
            else:
                cells.append("—")
        worst = max(deltas, key=abs) if deltas else None
        a24 = _thresholds(awgn, width=24).get(r.mcs)
        lines.append(f"| {r.mcs} | {r.modulation} {r.code_rate} | {params.clip_db(r.mcs):.0f} | "
                     f"{r.nominal_snr_db:.0f} | " + " | ".join(cells)
                     + f" | {_fmt(worst, 1) if worst is not None else '—'} | "
                     + (f"{_round_up_half(a24):.1f}" if a24 is not None else "—") + " |")
    lines += ["", "Each rung runs at its own clip level (ADR 0003: 7 dB at MCS 0–3, then 8, 9 "
              "and 10 dB), and every threshold is the 7 dB-equivalent SNR: the average-power "
              "threshold plus the clip level minus 7 dB, so rungs compare at equal peak level. "
              "\"Table value\" is the AWGN 24-tone threshold rounded up to the next 0.5 dB, "
              "the figure for the SPEC 6.2 Threshold column. \"floor x%\" means BLER stopped "
              "falling well above 1%; x is the lowest BLER seen. \"fails\" means BLER stayed "
              "above 50%. Worst Δ is over the measured thresholds only.", "",
              f"![AWGN, 24 tones]({bler_plot(awgn, 'AWGN, 24 tones', 'bler_awgn_24.png', width=24)})", ""]
    if chans:
        for prof in ("mic", "flat12", "flat25", "repeater"):
            if any(r["profile"] == prof for r in chans):
                lines += [f"![{prof}]({bler_plot(chans, prof, f'bler_{prof}.png', profile=prof)})", ""]
    return lines


def clip_plot(rows: list[dict]) -> str:
    PLOTS.mkdir(parents=True, exist_ok=True)
    fig, ax = plt.subplots(figsize=(6, 3.6))
    for mcs in sorted({r["mcs"] for r in rows}):
        pts = sorted((r["clip_db"], r["threshold_db"] + r["clip_db"] - 7.0) for r in rows
                     if r["mcs"] == mcs and r["threshold_db"] is not None and not _floor(r))
        if pts:
            ax.plot(*zip(*pts), "o-", label=f"MCS {mcs}")
    ax.set_xlabel("clip level (dB above RMS)")
    ax.set_ylabel("peak-referenced threshold (dB)")
    ax.set_title("Threshold + (clip - 7 dB); lower is better")
    ax.grid(True, alpha=0.3)
    ax.legend(fontsize=7)
    fig.tight_layout()
    fig.savefig(PLOTS / "clip.png", dpi=110)
    plt.close(fig)
    return "results/plots/clip.png"


def clip_section() -> list[str]:
    rows = _load("clip")
    lines = ["## Clip level against rung threshold (AWGN, 24 tones)", ""]
    if rows is None:
        return lines + ["Clip sweep not yet run.", ""]
    clips = sorted({r["clip_db"] for r in rows})
    lines += ["Thresholds are in audio SNR (average power). On a peak-limited FM channel each "
              "dB of extra clip level costs a dB of average power, so the last column adds "
              "that penalty back relative to the 7 dB clip.", "",
              "| MCS | " + " | ".join(f"clip {c:.0f} dB" for c in clips) + " | Best peak-referenced |",
              "|" + " --- |" * (len(clips) + 2)]
    for mcs in sorted({r["mcs"] for r in rows}):
        mine = {r["clip_db"]: r for r in rows if r["mcs"] == mcs}
        vals = {c: (None if _floor(r) else r["threshold_db"]) for c, r in mine.items()}
        peak_ref = {c: v + (c - 7.0) for c, v in vals.items() if v is not None}
        best = min(peak_ref.items(), key=lambda kv: kv[1]) if peak_ref else None

        def cell(c):
            if c not in mine:
                return ""
            if vals[c] is not None:
                return f"{vals[c]:.1f}"
            return f"floor {100 * min(p['bler'] for p in mine[c]['points']):.1f}%"
        lines.append(f"| {mcs} | " + " | ".join(cell(c) for c in clips)
                     + f" | {f'{best[1]:.1f} dB at clip {best[0]:.0f}' if best else '—'} |")
    lines.append("")
    lines += [f"![Clip sweep]({clip_plot(rows)})", ""]
    return lines


def interleaver_section() -> list[str]:
    rows = _load("interleaver")
    lines = ["## Interleaver against a random permutation (SPEC 6.4)", ""]
    if rows is None:
        return lines + ["Interleaver comparison not yet run.", ""]
    lines += ["AWGN, 2 blocks per frame, BLER at each SNR (dB). \"Dead tone\" is one data tone "
              "lost in the channel without being masked; \"burst\" is 10 ms at +20 dB, received "
              "with `receive(burst_aware=True)`. Structural metrics are in "
              "`cairn_sim/interleaver_eval.py`.", "",
              "| Impairment | MCS | Tones | SNR (dB) | Linear (SPEC 6.4) | Random | Unimpaired |",
              "| --- | --- | --- | --- | --- | --- | --- |"]

    def find(imp, mcs, w, kind):
        return next((r for r in rows if (r["impairment"], r["mcs"], r["width"], r["interleaver"])
                     == (imp, mcs, w, kind)), None)

    def fmt(r, snrs):
        if r is None:
            return ""
        by = {p["snr_db"]: p["bler"] for p in r["points"]}
        return " / ".join(f"{100 * by[s]:.2f}%" if s in by else "—" for s in snrs)

    for imp in ("dead_tone", "burst"):
        for mcs in (2, 4):
            for w in (24, 56):
                lin = find(imp, mcs, w, "linear")
                if lin is None:
                    continue
                snrs = [p["snr_db"] for p in sorted(lin["points"], key=lambda p: p["snr_db"])]
                base = find("none", mcs, w, "linear") if imp == "dead_tone" else None
                lines.append(f"| {imp.replace('_', ' ')} | {mcs} | {w} | "
                             f"{' / '.join(f'{s:g}' for s in snrs)} | {fmt(lin, snrs)} | "
                             f"{fmt(find(imp, mcs, w, 'random'), snrs)} | {fmt(base, snrs)} |")
    return lines + [""]


def header_section() -> list[str]:
    rows = _load("header")
    lines = ["## Header frame loss at MCS 0 (SPEC 6.3)", ""]
    if rows is None:
        return lines + ["Header comparison not yet run.", ""]
    lines += ["Fraction of frames lost at the header (preamble missed or header CRC failed), "
              "24 tones, 2 data blocks per frame, the same frames for both estimates. \"s2\" "
              "is the earlier per-tone noise estimate from the S2 pair; \"measured\" (the "
              "default) adds S1 and the header symbols' own error.", "",
              "| Channel | Header noise | " , "| --- | --- |"]
    snr_cols = {}
    for r in rows:
        snr_cols.setdefault(r["profile"], sorted(p["snr_db"] for p in r["points"]))
    width = max(len(v) for v in snr_cols.values())
    lines[-2] += " | ".join(f"SNR {i + 1}" for i in range(width)) + " |"
    lines[-1] += " --- |" * width
    for r in rows:
        pts = sorted(r["points"], key=lambda p: p["snr_db"])
        lines.append(f"| {r['profile']} | {r['rx_kw']['header_noise']} | "
                     + " | ".join(f"{p['header_fer'] * 100:.2f}% at {p['snr_db']:g} dB" for p in pts) + " |")
    return lines + [""]


def main() -> int:
    parts = ["# Stage 1 simulation report", "",
             "Generated by `python -m cairn_sim.report` from `sim/results/`. Audio SNR is "
             "defined in `sim/README.md`.", ""]
    findings = SIM / "findings.md"
    if findings.exists():
        parts += [findings.read_text().strip(), ""]
    parts += (thresholds_section() + clip_section() + header_section() + interleaver_section()
              + papr_section() + budget_section())
    for extra in ("ldpc", "preamble", "channel"):
        f = RESULTS / f"{extra}.md"
        if f.exists():
            parts += [f.read_text().strip(), ""]
    (SIM / "report.md").write_text("\n".join(parts))
    print(f"wrote {SIM / 'report.md'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
