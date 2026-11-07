"""``play``, ``record``, ``levels``, ``ptt-test`` and ``devices``: sound card and PTT convenience.

Optional: needs ``sounddevice`` (audio) and ``pyserial`` (PTT), installed with
``uv sync --package cairn-sim --extra audio``. Nothing else in the bench kit
imports them. Without these, play and record with any audio tool and use VOX.

PTT is a serial control line. The default matches the AIOC (All-In-One-Cable):
DTR asserted and RTS deasserted keys the radio. Both lines are forced low
*before* the port opens (a plain ``Serial(port)`` asserts DTR on open, which
would key the radio), and the transmitter is released on every way out:
normal return, an exception, Ctrl-C, SIGTERM, SIGHUP and interpreter exit.
"""

from __future__ import annotations

import argparse
import atexit
import signal
import sys
import time

import numpy as np

from . import audio

CLIP_LEVEL = 0.999


class MissingDependency(RuntimeError):
    pass


def _sd():
    try:
        import sounddevice
    except ImportError as e:      # pragma: no cover - depends on the environment
        raise MissingDependency("sounddevice is not installed: uv sync --package cairn-sim --extra audio "
                                "(and PortAudio: libportaudio2 on Debian/Ubuntu, portaudio on Arch)") from e
    return sounddevice


def _serial():
    try:
        import serial
    except ImportError as e:      # pragma: no cover
        raise MissingDependency("pyserial is not installed: uv sync --package cairn-sim --extra audio") from e
    return serial


# --- levels -----------------------------------------------------------------


def level_stats(x: np.ndarray) -> dict:
    """RMS, peak and DC offset of mono float audio, in dBFS (DC also as a fraction of full scale)."""
    x = np.asarray(x, dtype=float)
    if x.size == 0:
        raise ValueError("no samples")
    dc = float(np.mean(x))
    ac = x - dc
    return {"samples": int(x.size), "rms_dbfs": audio.db(float(np.sqrt(np.mean(x ** 2)))),
            "ac_rms_dbfs": audio.db(float(np.sqrt(np.mean(ac ** 2)))),
            "peak_dbfs": audio.db(float(np.max(np.abs(x)))), "dc": dc, "dc_dbfs": audio.db(abs(dc)),
            "clipped_samples": int(np.sum(np.abs(x) >= CLIP_LEVEL))}


def format_levels(s: dict) -> str:
    out = (f"RMS {s['rms_dbfs']:.1f} dBFS (AC {s['ac_rms_dbfs']:.1f}), peak {s['peak_dbfs']:.1f} dBFS, "
           f"DC {s['dc'] * 100:+.2f}% of full scale ({s['dc_dbfs']:.1f} dBFS), {s['samples']} samples")
    if s["clipped_samples"]:
        out += f", CLIPPED {s['clipped_samples']} samples: lower the receive level"
    return out


# --- PTT --------------------------------------------------------------------


class Ptt:
    """Serial-line PTT with guaranteed release.

    ``line`` is the control line that keys ("dtr" or "rts"); the other line is
    held deasserted (the AIOC keys on DTR=1, RTS=0). ``invert`` makes the
    keyed state the deasserted one, for interfaces wired active low.
    """

    def __init__(self, port: str, line: str = "dtr", invert: bool = False, serial_module=None) -> None:
        if line not in ("dtr", "rts"):
            raise ValueError("PTT line must be 'dtr' or 'rts'")
        self.line, self.invert, self.port = line, invert, port
        mod = serial_module or _serial()
        self._s = mod.Serial()
        self._s.port = port
        self._s.dtr = False         # before open(): opening must never key the radio
        self._s.rts = False
        self._s.open()
        self._keyed = False
        self._old: dict = {}
        atexit.register(self.release)
        self._set(False)

    @property
    def keyed(self) -> bool:
        return self._keyed

    def _set(self, on: bool) -> None:
        level = on != self.invert
        other = "rts" if self.line == "dtr" else "dtr"
        setattr(self._s, other, False)
        setattr(self._s, self.line, level)

    def key(self) -> None:
        self._set(True)
        self._keyed = True

    def unkey(self) -> None:
        try:
            self._set(False)
        finally:
            self._keyed = False

    def release(self) -> None:
        """Unkey and close; safe to call twice."""
        try:
            if self._s.is_open:
                self.unkey()
                self._s.close()
        except Exception:         # pragma: no cover - last-ditch cleanup
            pass

    def __enter__(self) -> "Ptt":
        def handler(signum, frame):
            self.release()
            raise KeyboardInterrupt if signum == signal.SIGINT else SystemExit(128 + signum)

        for name in ("SIGINT", "SIGTERM", "SIGHUP"):
            sig = getattr(signal, name, None)
            if sig is not None:
                try:
                    self._old[sig] = signal.signal(sig, handler)
                except ValueError:        # not the main thread
                    pass
        return self

    def __exit__(self, *exc) -> None:
        try:
            self.release()
        finally:
            for sig, h in self._old.items():
                signal.signal(sig, h)
            self._old.clear()
            try:
                atexit.unregister(self.release)
            except Exception:
                pass


# --- audio devices ----------------------------------------------------------


def find_device(name: str | None, kind: str, sd=None):
    """Device index by case-insensitive name substring (``None``: the default device)."""
    sd = sd or _sd()
    if name is None:
        return None
    chan = "max_output_channels" if kind == "output" else "max_input_channels"
    devs = [(i, d) for i, d in enumerate(sd.query_devices()) if d[chan] > 0]
    hits = [i for i, d in devs if name.lower() in d["name"].lower()]
    if not hits:
        names = ", ".join(repr(d["name"]) for _, d in devs) or "none"
        raise SystemExit(f"no {kind} device matching {name!r}; available: {names}")
    return hits[0]


def play_audio(x: np.ndarray, fs: int, device, *, ptt: Ptt | None = None, txdelay: float = 0.3,
               txtail: float = 0.2, sd=None) -> None:
    """Play mono ``x``; if ``ptt`` is given key it ``txdelay`` s before the audio and release
    ``txtail`` s after, whatever happens."""
    sd = sd or _sd()
    try:
        if ptt is not None:
            ptt.key()
            time.sleep(txdelay)
        sd.play(x.astype("float32"), fs, device=device)
        sd.wait()
        if ptt is not None:
            time.sleep(txtail)
    finally:
        if ptt is not None:
            ptt.unkey()


def record_audio(seconds: float, fs: int, device, channels: int = 1, sd=None) -> np.ndarray:
    sd = sd or _sd()
    rec = sd.rec(int(round(seconds * fs)), samplerate=fs, channels=channels, device=device, dtype="float32")
    sd.wait()
    return np.asarray(rec, dtype=float)


# --- commands ---------------------------------------------------------------


def _ptt_from(a: argparse.Namespace) -> Ptt | None:
    if not a.ptt_port:
        return None
    return Ptt(a.ptt_port, a.ptt_line, a.ptt_invert)


def cmd_devices(a: argparse.Namespace) -> int:
    for i, d in enumerate(_sd().query_devices()):
        print(f"{i:3d}  in {d['max_input_channels']}  out {d['max_output_channels']}  "
              f"{d['default_samplerate']:.0f} Hz  {d['name']}")
    return 0


def cmd_play(a: argparse.Namespace) -> int:
    fs, data = audio.read_wav(a.wav)
    x = audio.to_mono(data, "auto")[0]
    dev = find_device(a.device, "output")
    dur = len(x) / fs
    if a.dry_run:
        print(f"dry run: would {'key ' + a.ptt_port + ' (' + a.ptt_line + ') ' if a.ptt_port else 'not key PTT, '}"
              f"wait {a.txdelay:.2f} s, play {dur:.1f} s to {a.device or 'the default device'}, "
              f"hold {a.txtail:.2f} s")
        return 0
    if dur > 120:
        print(f"warning: {dur:.0f} s is longer than many radios' transmit time-out", file=sys.stderr)
    ptt = _ptt_from(a)
    try:
        if ptt is not None:
            with ptt:
                play_audio(x, fs, dev, ptt=ptt, txdelay=a.txdelay, txtail=a.txtail)
        else:
            play_audio(np.concatenate([np.zeros(int(a.txdelay * fs)), x]), fs, dev)
    finally:
        if ptt is not None:
            ptt.release()
    print(f"played {dur:.1f} s")
    return 0


def cmd_record(a: argparse.Namespace) -> int:
    dev = find_device(a.device, "input")
    x = record_audio(a.seconds, a.rate, dev, a.channels)
    if a.out:
        if a.channels == 1:
            audio.write_wav(a.out, x[:, 0], a.rate)
        else:
            _write_multi(a.out, x, a.rate)
        print(f"wrote {a.out}")
    print("level check: " + format_levels(level_stats(x[:, 0])))
    return 0


def _write_multi(path: str, x: np.ndarray, fs: int) -> None:
    from scipy.io import wavfile
    wavfile.write(path, fs, np.clip(np.rint(x * 32768.0), -32768, 32767).astype("<i2"))


def cmd_levels(a: argparse.Namespace) -> int:
    dev = find_device(a.device, "input")
    for _ in range(a.repeat):
        x = record_audio(a.seconds, a.rate, dev, 1)
        print(format_levels(level_stats(x[:, 0])), flush=True)
    return 0


def cmd_ptt_test(a: argparse.Namespace) -> int:
    if a.seconds > 5 and not a.force:
        print("ptt-test: refusing to key for more than 5 s without --force", file=sys.stderr)
        return 2
    with Ptt(a.ptt_port, a.ptt_line, a.ptt_invert) as ptt:
        print(f"keying {a.ptt_port} ({a.ptt_line}{', inverted' if a.ptt_invert else ''}) for {a.seconds:.1f} s")
        ptt.key()
        time.sleep(a.seconds)
        ptt.unkey()
    print("released")
    return 0


def _ptt_args(p: argparse.ArgumentParser, required: bool = False) -> None:
    p.add_argument("--ptt-port", required=required, help="serial port that keys the radio, e.g. "
                   "/dev/serial/by-id/usb-AIOC_All-In-One-Cable_*-if04 (omit to rely on VOX)")
    p.add_argument("--ptt-line", default="dtr", choices=["dtr", "rts"],
                   help="control line that keys (default dtr; the other is held low, as the AIOC wants)")
    p.add_argument("--ptt-invert", action="store_true", help="active low: key by deasserting the line")


def add_commands(sub) -> None:
    d = sub.add_parser("devices", help="list audio devices")
    d.set_defaults(fn=cmd_devices)

    p = sub.add_parser("play", help="play a WAV, keying PTT around it (needs the audio extra)")
    p.add_argument("wav")
    p.add_argument("--device", help="output device, by name substring (e.g. All-In-One-Cable)")
    _ptt_args(p)
    p.add_argument("--txdelay", type=float, default=0.3, help="seconds between keying and audio (default 0.3)")
    p.add_argument("--txtail", type=float, default=0.2, help="seconds of carrier after the audio (default 0.2)")
    p.add_argument("--dry-run", action="store_true", help="show what would happen; touch nothing")
    p.set_defaults(fn=cmd_play)

    r = sub.add_parser("record", help="record from a sound device to a WAV and print a level check")
    r.add_argument("--device", help="input device, by name substring")
    r.add_argument("--seconds", type=float, required=True)
    r.add_argument("-o", "--out", help="output WAV (omit for a level check only)")
    r.add_argument("--rate", type=int, default=48000, help="sample rate (default 48000)")
    r.add_argument("--channels", type=int, default=1)
    r.set_defaults(fn=cmd_record)

    lv = sub.add_parser("levels", help="level check: RMS, peak and DC of the input, repeated")
    lv.add_argument("--device", help="input device, by name substring")
    lv.add_argument("--seconds", type=float, default=2.0)
    lv.add_argument("--repeat", type=int, default=1)
    lv.add_argument("--rate", type=int, default=48000)
    lv.set_defaults(fn=cmd_levels)

    t = sub.add_parser("ptt-test", help="key PTT for a short time, then release")
    _ptt_args(t, required=True)
    t.add_argument("--seconds", type=float, default=0.5)
    t.add_argument("--force", action="store_true", help="allow keying longer than 5 s")
    t.set_defaults(fn=cmd_ptt_test)
