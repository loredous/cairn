"""``cairn-bench`` command line (see docs/bench-testing.md)."""

from __future__ import annotations

import argparse
import os
import sys

from . import rx, tx


def _manifest_path(wav: str) -> str:
    return os.path.splitext(wav)[0] + ".json"


def cmd_tx(a: argparse.Namespace) -> int:
    if a.list_plans:
        for k, v in tx.PLANS.items():
            print(f"{k:9s} {v}")
        return 0
    if not a.out:
        print("tx: -o/--out is required", file=sys.stderr)
        return 2
    specs = tx.make_plan(a.plan, rungs=tx.parse_ints(a.rungs), widths=tx.parse_ints(a.widths),
                         prefix=a.prefix, silenced=tx.parse_tones(a.mask) if a.mask else (),
                         frame_s=a.frame_s, blocks=a.blocks)
    x, manifest = tx.synthesize(specs, repeat=a.repeat, seed=a.seed, lead_s=a.lead, tone_s=a.tone_s,
                                gap_s=a.gap, callsign=a.callsign, wpm=a.wpm, id_interval_s=a.id_every,
                                peak_dbfs=a.peak_dbfs)
    mpath = a.manifest or _manifest_path(a.out)
    tx.write(a.out, mpath, x, manifest)
    print(f"wrote {a.out} and {mpath}")
    print(tx.describe(manifest))
    if not a.callsign:
        print("  note: no --callsign given, so no CW identification is in this file; "
              "identify yourself another way when transmitting on the air")
    return 0


def cmd_rx(a: argparse.Namespace) -> int:
    import json
    mpath = a.manifest or _manifest_path(a.wav)
    with open(mpath) as f:
        manifest = json.load(f)
    x, info = rx.load(a.wav, a.channel)
    collect: dict = {}
    rep = rx.analyze(x, manifest, rho=a.rho, correct_ppm=not a.no_ppm_correct, collect=collect)
    rep = {"recording": info, **rep}
    print(rx.format_report(rep))
    if a.json:
        text = rx.to_json(rep)
        if a.json == "-":
            print(text)
        else:
            with open(a.json, "w") as f:
                f.write(text)
    if a.plots:
        for p in rx.plots(rep, collect, a.plots, a.plot_frame):
            print(f"plot: {p}")
    return 0 if not rep["summary"]["missed"] else 1


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="cairn-bench", description="Cairn bench test kit: WAV of test frames out, "
                                "recording in, offline decode and report.")
    sub = p.add_subparsers(dest="cmd", required=True)

    t = sub.add_parser("tx", help="write a test WAV and its manifest")
    t.add_argument("-o", "--out", help="output WAV (48 kHz mono 16-bit); the manifest goes next to it as .json")
    t.add_argument("--manifest", help="manifest path (default: the WAV name with .json)")
    t.add_argument("--plan", default="quick", choices=sorted(tx.PLANS), help="frame plan (default quick)")
    t.add_argument("--list-plans", action="store_true", help="list the plans and exit")
    t.add_argument("--rungs", default="0,2,4", help="custom plan: MCS list, e.g. 0-3,6")
    t.add_argument("--widths", default="24", help="custom plan: tone widths (24-56), e.g. 24,40")
    t.add_argument("--prefix", default="short", choices=["short", "long", "both"], help="custom plan prefix")
    t.add_argument("--mask", help="tones to silence in data symbols, e.g. 8-11,30 (never 3 or 20)")
    t.add_argument("--repeat", type=int, default=1, help="repeat the whole plan N times")
    t.add_argument("--frame-s", type=float, help="target frame time in seconds (default: SPEC 10.2 per rung)")
    t.add_argument("--blocks", type=int, help="data blocks per frame, overriding the frame time")
    t.add_argument("--gap", type=float, default=0.5, help="silence between frames, s (default 0.5; keep it "
                   "shorter than a VOX hang time)")
    t.add_argument("--lead", type=float, default=1.0, help="leading silence, s (default 1)")
    t.add_argument("--tone-s", type=float, default=5.0, help="level-setting tone length, s; 0 for none (default 5)")
    t.add_argument("--seed", type=int, default=1, help="payload seed (default 1)")
    t.add_argument("--peak-dbfs", type=float, default=-3.0, help="peak level of everything, dBFS (default -3)")
    t.add_argument("--callsign", help="send this as a CW ID at the start, the end and at least every 10 minutes")
    t.add_argument("--wpm", type=float, default=20.0, help="CW speed (default 20)")
    t.add_argument("--id-every", type=float, default=tx.ID_INTERVAL_S, help="maximum seconds between CW IDs")
    t.set_defaults(fn=cmd_tx)

    r = sub.add_parser("rx", help="decode a recording and report per frame")
    r.add_argument("wav", help="recording (any sample rate, mono or stereo)")
    r.add_argument("--manifest", help="manifest from tx (default: the WAV name with .json)")
    r.add_argument("--json", metavar="PATH", help="write the full report as JSON ('-' for stdout)")
    r.add_argument("--plots", metavar="DIR", help="write per-tone SNR and constellation plots here")
    r.add_argument("--plot-frame", type=int, help="frame index for the constellation (default: highest rung decoded)")
    r.add_argument("--channel", default="auto", help="stereo input: auto (loudest), left, right, mix or an index")
    r.add_argument("--rho", type=float, default=0.55, help="preamble detection threshold (default 0.55)")
    r.add_argument("--no-ppm-correct", action="store_true", help="do not resample the recording by the fitted clock offset")
    r.set_defaults(fn=cmd_rx)

    from . import devices
    devices.add_commands(sub)
    return p


def main(argv: list[str] | None = None) -> int:
    a = build_parser().parse_args(argv)
    return a.fn(a)


if __name__ == "__main__":
    sys.exit(main())
