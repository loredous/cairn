"""Channel models (SPEC 3.1, 3.2, 14.2).

Audio SNR, as used throughout the simulation: the signal's mean power over
the noise power in the occupied band (300 Hz to the top active tone + 50 Hz),
both measured at the receiver's audio output. With 100 Hz tones filling that
band, audio SNR equals the per-tone SNR the demodulator sees.

Two models live here:

* ``awgn`` adds white noise at a set audio SNR and nothing else.
* ``FmChannel`` is the SPEC 14.2 audio chain of two FM radios, in this order:

  1. FM transmit pre-emphasis (mic path)
  2. mic-path band-pass, then the radio's limiter (soft knee)
  3. deviation clipping (hard)
  4. additive noise at the set audio SNR
  5. FM receive noise tilt (applied to the noise only)
  6. de-emphasis (mic path)
  7. receive band-pass
  8. sample-clock offset (resampling by 1 + eps)
  9. DC offset and 60 Hz hum

  The repeater profile runs two such hops (stages 1 to 7) with the
  repeater controller's band-pass and an all-pass dispersion network
  between them, then stages 8 and 9 once.

Levels (normalized units)
-------------------------
The radio's limiter knee is amplitude 1.0. ``drive_db`` scales the modem's
waveform so its peak sample is ``drive_db`` relative to the knee: default
-3 dB, the calibrated target of SPEC 7.1 / 13.5 / 20.6 (peaks 3 dB below the
knee). The transmit filters are normalized to unity gain at 1000 Hz, the
level-setting tone, so "relative to the knee" means relative to a 1 kHz tone
that just reaches it, which is what the stepped-tone calibration of 13.5
measures. On the mic path the pre-emphasis therefore lifts the upper tones
above the drive level (+8.5 dB at 2650 Hz), as on a real radio. The output is
left in the same units (passband gain about 1 at 1 kHz); the receiver's AGC
is not modelled.

* Limiter (mic path): linear up to the knee T = 1, then
  ``T + (L - T) tanh((|x| - T) / (L - T))``, which leaves the knee with unit
  slope and saturates at the ceiling L = +3 dB (1.413). A soft knee is what
  the 13.5 calibration looks for (gain falling below 0.5 dB/dB).
* Deviation clip: a hard clip at the radio's maximum deviation. Mic path:
  +2 dB above the knee (1.259), so heavy overdrive is cut off before the
  limiter's soft region is used up. Flat ports have no limiter; the clip at
  1.0 is the knee (a tone's output stops rising there), so the default drive
  puts data peaks at 70% of maximum deviation, as SPEC 7.1 asks.

Filters and their parameters
----------------------------
All filters are ``scipy.signal`` second-order sections at 48 kHz; analog
prototypes are bilinear-transformed with their corners pre-warped.

* Pre-emphasis: the NBFM 750 us network, H(s) = (1 + s tau1)/(1 + s tau2),
  tau1 = 750 us (zero at 212 Hz, so +6 dB/octave across the voice band) and
  a pole at 8 kHz (tau2 = 19.9 us) where the real network's series resistor
  stops the rise. Unity gain at 1 kHz.
* De-emphasis: the plain RC 750 us low-pass, 1/(1 + s tau1), unity at 1 kHz.
  It is deliberately not the exact inverse: the transmitter's 8 kHz pole
  remains, so the net response falls 0.5 dB by 2.7 kHz and 0.6 dB by 3 kHz
  (SPEC 3.1: "net roughly flat").
* Mic transmit band-pass: Butterworth high-pass 300 Hz order 2 (CTCSS and
  rumble rejection) and low-pass 3000 Hz order 6 (splatter filter).
* Mic receive band-pass: the same corners and orders (CTCSS high-pass and
  speaker-audio low-pass). Corners are the -3 dB points. The orders are set
  by the dispersion SPEC 3.2 budgets for a simplex link, "about 1-2 ms":
  this pair gives 1.1 ms of group-delay spread across 350-2650 Hz
  (high-pass order 3 would give 2.0 ms, order 4 would give 2.9 ms), with a
  roll-off of 3.7 dB at the lowest tone and 1.8 dB at the highest that the
  per-tone equalizer removes. Even so, the high-pass tails reach past the
  1.25 ms prefix: noise-free, with the receiver's default window advance,
  tones 0, 1 and 2 see about 21, 25 and 28 dB signal-to-ISI.
* Flat ports: no transmit filtering (the 9600 input feeds the modulator
  directly). Receive: a first-order 30 Hz high-pass (AC coupling) and an
  order 8 Butterworth low-pass standing for the IF filter that caps the
  discriminator's audio band, at 3300 Hz on 12.5 kHz channels (``flat12``)
  and 5900 Hz on 25 kHz channels (``flat25``) (SPEC 3.1 table).
* Noise tilt: the discriminator's noise amplitude rises 6 dB/octave
  (PSD proportional to f^2), modelled as a first difference 1 - z^-1
  (within 0.2 dB of an ideal differentiator up to 6 kHz), unity at 1 kHz.
  It is applied on every FM profile: on the mic path the de-emphasis then
  flattens it again above about 300 Hz, which is the point of emphasis; on
  flat ports it reaches the output, so upper tones get lower SNR.
* Repeater controller band-pass: Butterworth high-pass 300 Hz order 2
  (strips the input CTCSS) and low-pass 3000 Hz order 4.
* Repeater dispersion: one second-order all-pass section centred at 500 Hz
  whose Q is solved so the group-delay spread across the 24-tone band
  (300-2700 Hz) is exactly 2.0 ms (Q comes out at 1.54): delay humps near the
  bottom of the band are what controller and CTCSS filters produce.
* Repeater noise: SPEC 11.5 says two FM hops cost about 3 dB, so each hop
  contributes half of the output noise: hop 1 is set to the requested SNR
  + 3 dB at the repeater's receiver output, and hop 2 tops the noise up so
  the station's output SNR is exactly the request. The repeater retransmits
  at unity gain (a 1 kHz tone at 70% deviation in leaves at 70% out).

Other impairments
-----------------
* Sample-clock offset: ``y[n] = x((1 + eps) n)`` with eps = ``sco_ppm`` *
  1e-6, so a tone at f leaves at f (1 + eps) and the output is shorter by
  the factor 1 + eps. The interpolator is a 32-tap Kaiser-windowed sinc
  (beta 9, cutoff 0.4 fs) taken from a 4096-phase table at the nearest
  phase: against the analytic waveform its error is -108 dB at 300 Hz,
  -92 dB at 2650 Hz and -85 dB at 6 kHz, and a 4 s frame takes about
  35 ms. SPEC 14.1 targets +-100 ppm.
* DC offset (``dc``, normalized units, default 0.01 = 1% of the knee) and
  60 Hz hum (``hum_db``, hum power relative to the output in-band signal
  power, default -30 dB, random phase); the ideal profile defaults to none.
  Both sit below the occupied band, so they do not count in the audio SNR.

Exact audio SNR
---------------
Everything after the noise is linear, so the signal and the noise run
through the post-noise stages separately, their in-band powers are measured
at the output, and the noise is scaled so the in-band SNR is exactly
``snr_db``. The signal's mean power is taken over its active span (first to
last non-zero input sample), so zero padding around a frame does not dilute
it; the noise is stationary and is averaged over the whole output.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from functools import cache

import numpy as np
from scipy import fft as sfft
from scipy import optimize, signal

from . import params

FS = params.FS


def occupied_band(width: int) -> tuple[float, float]:
    return params.BAND_LOW_HZ, params.band_high_hz(width)


def awgn(x: np.ndarray, snr_db: float, width: int, rng: np.random.Generator,
         signal_power: float | None = None) -> np.ndarray:
    """Add white Gaussian noise so the in-band audio SNR is ``snr_db``."""
    lo, hi = occupied_band(width)
    p_sig = float(np.mean(x ** 2)) if signal_power is None else signal_power
    in_band_noise = p_sig / 10 ** (snr_db / 10)
    variance = in_band_noise * (params.FS / 2) / (hi - lo)
    return x + rng.standard_normal(x.shape) * np.sqrt(variance)


# --- measurement --------------------------------------------------------------


def _band_spectra(width: int, *xs: np.ndarray) -> tuple[list[np.ndarray], int]:
    n = len(xs[0])
    n_fft = sfft.next_fast_len(n, real=True)
    f = np.fft.rfftfreq(n_fft, 1 / FS)
    lo, hi = occupied_band(width)
    band = (f >= lo) & (f <= hi)
    return [sfft.rfft(x, n_fft)[band] for x in xs], n_fft


def in_band_energy(x: np.ndarray, width: int) -> float:
    """Energy (sum of squares) of ``x`` inside the occupied band."""
    (spec,), n_fft = _band_spectra(width, x)
    return 2.0 * float(np.sum(np.abs(spec) ** 2)) / n_fft


def in_band_power(x: np.ndarray, width: int) -> float:
    """Mean power of ``x`` inside the occupied band, averaged over all of ``x``."""
    return in_band_energy(x, width) / len(x)


# --- stages -----------------------------------------------------------------


def soft_limit(x: np.ndarray, knee: float = 1.0, ceiling: float = 10 ** (3 / 20)) -> np.ndarray:
    """Radio limiter: linear to ``knee``, then a tanh curve saturating at ``ceiling``."""
    a = np.abs(x)
    span = ceiling - knee
    over = knee + span * np.tanh((a - knee) / span)
    return np.where(a <= knee, x, np.sign(x) * over)


def hard_clip(x: np.ndarray, level: float) -> np.ndarray:
    """Deviation clip: the modulator cannot exceed maximum deviation."""
    return np.clip(x, -level, level)


def _warp(f_hz: float) -> float:
    """Analog frequency that the bilinear transform maps to ``f_hz``."""
    return FS / np.pi * np.tan(np.pi * f_hz / FS)


def _normalize(sos: np.ndarray, f_hz: float = 1000.0) -> np.ndarray:
    _, h = signal.sosfreqz(sos, worN=[f_hz], fs=FS)
    sos = sos.copy()
    sos[0, :3] /= abs(h[0])
    return sos


EMPHASIS_TAU_S = 750e-6
PREEMPHASIS_POLE_HZ = 8000.0


def _first_order_sos(zero_hz: float | None, pole_hz: float) -> np.ndarray:
    """Bilinear (1 + s/wz) / (1 + s/wp) with pre-warped corners, unity at 1 kHz."""
    wp = 2 * np.pi * _warp(pole_hz)
    if zero_hz is None:
        z, p, k = [], [-wp], wp
    else:
        wz = 2 * np.pi * _warp(zero_hz)
        z, p, k = [-wz], [-wp], wp / wz
    zd, pd, kd = signal.bilinear_zpk(z, p, k, FS)
    return _normalize(signal.zpk2sos(zd, pd, kd))


@cache
def preemphasis_sos() -> np.ndarray:
    """750 us pre-emphasis (zero at 212 Hz) with its pole at 8 kHz."""
    return _first_order_sos(1 / (2 * np.pi * EMPHASIS_TAU_S), PREEMPHASIS_POLE_HZ)


@cache
def deemphasis_sos() -> np.ndarray:
    """750 us RC de-emphasis (pole at 212 Hz)."""
    return _first_order_sos(None, 1 / (2 * np.pi * EMPHASIS_TAU_S))


@cache
def noise_tilt_sos() -> np.ndarray:
    """Discriminator noise shaping: first difference, +6 dB/octave, unity at 1 kHz."""
    return _normalize(np.array([[1.0, -1.0, 0.0, 1.0, 0.0, 0.0]]))


@dataclass(frozen=True)
class Band:
    """Butterworth band-pass: high-pass and low-pass -3 dB corners (None: absent)."""
    lo_hz: float | None
    hi_hz: float | None
    lo_order: int = 2
    hi_order: int = 6

    @property
    def sos(self) -> np.ndarray:
        return _band_sos(self)


@cache
def _band_sos(band: Band) -> np.ndarray:
    parts = []
    if band.lo_hz is not None:
        parts.append(signal.butter(band.lo_order, band.lo_hz, "highpass", fs=FS, output="sos"))
    if band.hi_hz is not None:
        parts.append(signal.butter(band.hi_order, band.hi_hz, "lowpass", fs=FS, output="sos"))
    return np.vstack(parts)


def _allpass_sos(f0_hz: float, q: float) -> np.ndarray:
    w0 = 2 * np.pi * f0_hz / FS
    alpha = np.sin(w0) / (2 * q)
    b = np.array([1 - alpha, -2 * np.cos(w0), 1 + alpha])
    a = np.array([1 + alpha, -2 * np.cos(w0), 1 - alpha])
    return np.concatenate([b / a[0], a / a[0]])[None, :]


def group_delay_s(sos: np.ndarray, f_hz: np.ndarray) -> np.ndarray:
    """Group delay of a cascade of sections, in seconds."""
    total = np.zeros(len(f_hz))
    for sec in sos:
        _, gd = signal.group_delay((sec[:3], sec[3:]), w=f_hz, fs=FS)
        total += gd
    return total / FS


def _spread(sos: np.ndarray, band: tuple[float, float]) -> float:
    f = np.linspace(band[0], band[1], 481)
    return float(np.ptp(group_delay_s(sos, f)))


@cache
def dispersion_sos(spread_s: float = 2e-3, f0_hz: float = 500.0,
                   band: tuple[float, float] = (300.0, 2700.0)) -> np.ndarray:
    """All-pass section with group-delay spread ``spread_s`` across ``band``."""
    q = optimize.brentq(lambda q: _spread(_allpass_sos(f0_hz, q), band) - spread_s, 0.05, 50.0,
                        xtol=1e-10)
    return _allpass_sos(f0_hz, q)


RESAMPLER_HALF_TAPS = 16
RESAMPLER_PHASES = 4096
RESAMPLER_CUTOFF = 0.4
RESAMPLER_BETA = 9.0


def resample(x: np.ndarray, ratio: float, half_taps: int = RESAMPLER_HALF_TAPS,
             phases: int = RESAMPLER_PHASES, cutoff: float = RESAMPLER_CUTOFF,
             beta: float = RESAMPLER_BETA) -> np.ndarray:
    """``y[n] = x(ratio * n)`` by windowed-sinc interpolation (zero outside ``x``).

    A tone at f comes out at f * ratio. The output has
    ``floor((len(x) - 1) / ratio) + 1`` samples. Each output uses the
    kernel phase nearest its fractional position.
    """
    if ratio == 1.0:
        return x.copy()
    table = _sinc_table(half_taps, phases, cutoff, beta)
    n_out = int(np.floor((len(x) - 1) / ratio)) + 1
    xp = np.concatenate([np.zeros(half_taps), x, np.zeros(half_taps + 1)])
    taps = np.arange(2 * half_taps)
    out = np.empty(n_out)
    chunk = 32768
    for s in range(0, n_out, chunk):
        t = np.arange(s, min(s + chunk, n_out)) * ratio
        base = np.floor(t).astype(np.int64)
        p = np.rint((t - base) * phases).astype(np.int64)   # 0..phases
        # Tap j of row m multiplies x[base + j - half_taps + 1].
        seg = xp[base[:, None] + 1 + taps[None, :]]
        out[s:s + len(t)] = np.einsum("ij,ij->i", seg, table[p])
    return out


@cache
def _sinc_table(half_taps: int, phases: int, cutoff: float, beta: float) -> np.ndarray:
    """Kernel rows for fractional delays p / phases, p = 0..phases (inclusive)."""
    frac = np.arange(phases + 1)[:, None] / phases
    k = np.arange(-half_taps + 1, half_taps + 1)[None, :]
    u = frac - k                                   # distance from output time to tap
    w = np.i0(beta * np.sqrt(np.clip(1 - (u / half_taps) ** 2, 0, None))) / np.i0(beta)
    h = 2 * cutoff * np.sinc(2 * cutoff * u) * w
    return h / h.sum(axis=1, keepdims=True)


# --- profiles -----------------------------------------------------------------

LIMITER_KNEE = 1.0
LIMITER_CEILING = 10 ** (3 / 20)
MIC_CLIP = 10 ** (2 / 20)
FLAT_CLIP = 1.0
HUM_HZ = 60.0
MIC_BAND = Band(300.0, 3000.0, 2, 6)


@dataclass(frozen=True)
class RadioPath:
    """One FM hop: transmitter audio stages, receiver audio stages."""
    emphasis: bool
    tx_band: Band | None
    limiter: bool
    clip: float
    rx_band: Band | None
    tilt: bool = True


@dataclass(frozen=True)
class Profile:
    name: str
    path: RadioPath | None           # None: ideal (AWGN only)
    hops: int = 1
    controller: Band | None = None   # between hops
    dispersion_s: float = 0.0        # between hops
    dc: float = 0.01
    hum_db: float | None = -30.0


MIC_PATH = RadioPath(emphasis=True, tx_band=MIC_BAND, limiter=True, clip=MIC_CLIP, rx_band=MIC_BAND)

PROFILES: dict[str, Profile] = {
    "mic": Profile("mic", MIC_PATH),
    "flat12": Profile("flat12", RadioPath(False, None, False, FLAT_CLIP, Band(30.0, 3300.0, 1, 8))),
    "flat25": Profile("flat25", RadioPath(False, None, False, FLAT_CLIP, Band(30.0, 5900.0, 1, 8))),
    "repeater": Profile("repeater", MIC_PATH, hops=2, controller=Band(300.0, 3000.0, 2, 4),
                        dispersion_s=2e-3),
    "ideal": Profile("ideal", None, dc=0.0, hum_db=None),
    # The mic path as the handhelds of the 2026-11-08 captures pass it: audio
    # to about 4.25 kHz (modem/fixtures/offair, w48), so 32 tones arrive
    # whole. Run at ``MIC_WIDE_DRIVE_DB`` its limiter leaves the distortion
    # those radios showed on their strong links.
    "mic_wide": Profile("mic_wide", RadioPath(emphasis=True, tx_band=Band(300.0, 4250.0, 2, 6), limiter=True,
                                              clip=MIC_CLIP, rx_band=Band(300.0, 4250.0, 2, 6))),
}
# The drive (peak against the limiter knee) at which ``mic_wide`` gives the
# 2026-11-08 strong links' distortion floor: at MCS 4, 32 tones, a reported
# SNR of 25 dB (on air: 26), 20.8 dB against every sent symbol (21.2), and
# error that peaks with the waveform (in-band residual kurtosis 21, the
# loudest 1% of milliseconds holding 17% of it; on air 11 and 15%).
MIC_WIDE_DRIVE_DB = -1.5


def _filt(sos: np.ndarray, x: np.ndarray) -> np.ndarray:
    return signal.sosfilt(sos, x)


def _tx(path: RadioPath | None, x: np.ndarray) -> np.ndarray:
    if path is None:
        return x
    if path.emphasis:
        x = _filt(preemphasis_sos(), x)
    if path.tx_band is not None:
        x = _filt(path.tx_band.sos, x)
    if path.limiter:
        x = soft_limit(x, LIMITER_KNEE, LIMITER_CEILING)
    return hard_clip(x, path.clip)


def _rx(path: RadioPath | None, x: np.ndarray) -> np.ndarray:
    if path is None:
        return x
    if path.emphasis:
        x = _filt(deemphasis_sos(), x)
    if path.rx_band is not None:
        x = _filt(path.rx_band.sos, x)
    return x


def _noise(path: RadioPath | None, n: int, rng: np.random.Generator) -> np.ndarray:
    """Receiver noise at the hop's audio output (before scaling)."""
    w = rng.standard_normal(n)
    if path is not None and path.tilt:
        w = _filt(noise_tilt_sos(), w)
    return _rx(path, w)


# --- weak-signal impairments (2026-11-08, 70 cm) ------------------------------
#
# Two models, both measured against the 70 cm captures of 2026-11-08 (see
# ``sim/findings.md``, "A weak 70 cm link"):
#
# * ``Comb``: additive lines at the harmonics of 1 kHz. That is what the
#   weak 70 cm direction actually carried: the lines arrive with the sending
#   station's carrier, at a constant level through the carrier lead-in, the
#   data and the carrier tail, and stop when the sender closes its audio
#   output stream although its carrier stays up; the receiver was quieted.
# * ``fm_discriminate``: complex-baseband FM with the carrier-to-noise ratio
#   as the parameter, the textbook model of a receiver near its FM threshold
#   (noise that rises 6 dB/octave above threshold, clicks below it). The
#   captures did not need it, but a genuinely weak link does.


@dataclass(frozen=True)
class Comb:
    """Additive lines at harmonics of ``f0_hz`` at the receiver's audio output.

    ``levels_dbc[k - 1]`` is the power of harmonic k relative to the signal's
    in-band power at the output (the same reference as ``hum_db``). Each line
    starts at a random phase. ``wander_db`` is the standard deviation of each
    line's slow level wander (a log-normal amplitude, low-passed to
    ``wander_hz``), as the captures show over 0.25 s windows; 0 holds the
    lines steady.
    """
    levels_dbc: tuple[float, ...]
    f0_hz: float = 1000.0
    wander_db: float = 0.0
    wander_hz: float = 4.0

    def scaled(self, offset_db: float) -> Comb:
        """The same comb, every line ``offset_db`` louder."""
        return Comb(tuple(v + offset_db for v in self.levels_dbc), self.f0_hz, self.wander_db, self.wander_hz)

    @property
    def total_dbc(self) -> float:
        """All lines together, relative to the in-band signal."""
        return float(10 * np.log10(np.sum(10 ** (np.asarray(self.levels_dbc) / 10))))

    def wave(self, n: int, signal_power: float, rng: np.random.Generator) -> np.ndarray:
        t = np.arange(n) / FS
        out = np.zeros(n)
        for k, lv in enumerate(self.levels_dbc, start=1):
            amp = np.sqrt(2 * signal_power * 10 ** (lv / 10))
            line = amp * np.cos(2 * np.pi * k * self.f0_hz * t + rng.uniform(0, 2 * np.pi))
            if self.wander_db > 0:
                line *= 10 ** (_wander(n, self.wander_hz, rng) * self.wander_db / 20)
            out += line
        return out


def _wander(n: int, f_hz: float, rng: np.random.Generator) -> np.ndarray:
    """Unit-variance Gaussian process, first-order low-passed to ``f_hz``."""
    a = np.exp(-2 * np.pi * f_hz / FS)
    w = signal.lfilter([np.sqrt(1 - a * a)], [1, -a], rng.standard_normal(n + 4 * int(FS / f_hz)))
    return w[-n:]


# The lines of the 2026-11-08 70 cm A->B setups (``weak_fm_eval captures``):
# 1-4 kHz, dB against the data's in-band power on the 7 dB-equivalent scale
# (ADR 0003; for a rung clipped at c dB, ``scaled(params.clip_offset_db(mcs))``
# puts them against that rung's lower average power), averaged over each
# setup's MCS 4, 5 and 6 frames. Their level held to 0.1-0.2 dB over 0.25 s
# windows, so they are modelled steady. Setup 1 is the first session, 2 a new
# antenna on the sender, 3 another radio, 4 that radio with the antenna
# moved; "70cm" is their mean shape at their mean level (-16.2 dB in all).
CAPTURE_COMBS: dict[str, Comb] = {
    "70cm-1": Comb((-23.6, -19.8, -19.8, -39.1)),
    "70cm-2": Comb((-28.2, -24.0, -23.1, -43.1)),
    "70cm-3": Comb((-25.4, -21.1, -20.2, -40.1)),
    "70cm-4": Comb((-20.7, -16.4, -15.4, -35.2)),
    "70cm": Comb((-24.5, -20.3, -19.6, -39.4)),
}

# FM receiver defaults: 25 kHz channel ("wide", as the captures), Carson's
# bandwidth for 5 kHz deviation and 3 kHz audio.
FM_MAX_DEV_HZ = 5000.0
FM_IF_BW_HZ = 16000.0
FM_IF_ORDER = 8


def fm_discriminate(audio: np.ndarray, cnr_db: float, rng: np.random.Generator, *,
                    full_scale: float = 1.0, max_dev_hz: float = FM_MAX_DEV_HZ,
                    if_bw_hz: float = FM_IF_BW_HZ) -> np.ndarray:
    """An FM link at carrier-to-noise ratio ``cnr_db``, in complex baseband.

    ``audio`` (the transmitter's output, ``full_scale`` = maximum deviation
    ``max_dev_hz``) frequency-modulates a unit carrier; complex Gaussian
    noise band-limited to the IF bandwidth ``if_bw_hz`` (order-8 Butterworth,
    -3 dB at +-if_bw/2) is added at power 1/CNR in that bandwidth, and a
    phase-difference discriminator returns the instantaneous frequency in the
    same units as ``audio``. The signal itself is not filtered: the IF passes
    it undistorted, so without noise the output equals ``audio`` exactly.
    Above threshold the output noise is Gaussian with a PSD rising as f^2;
    below it, the noise drags the phase around the origin and each 2 pi slip
    is a click (an impulse of area full_scale * fs / max_dev_hz).
    """
    f_inst = np.asarray(audio, dtype=float) * (max_dev_hz / full_scale)
    phase = 2 * np.pi * np.cumsum(f_inst) / FS
    n = len(phase)
    sos = signal.butter(FM_IF_ORDER, if_bw_hz / 2, "lowpass", fs=FS, output="sos")
    w = rng.standard_normal(n + 2048) + 1j * rng.standard_normal(n + 2048)
    w = (signal.sosfilt(sos, w.real) + 1j * signal.sosfilt(sos, w.imag))[2048:]
    w *= np.sqrt(10 ** (-cnr_db / 10) / np.mean(np.abs(w) ** 2))
    z = np.exp(1j * phase) + w
    dphi = np.angle(z[1:] * np.conj(z[:-1]))
    first = np.angle(z[0] * np.exp(-1j * (phase[0] - 2 * np.pi * f_inst[0] / FS)))
    return np.concatenate([[first], dphi]) * FS / (2 * np.pi) * (full_scale / max_dev_hz)


@dataclass
class ChannelOutput:
    """The pieces of one channel run; ``total`` is what ``FmChannel`` returns.

    ``signal`` is the noise-free output, ``noise`` everything the noise
    added (for the repeater, hop-1 noise after the repeater's limiter), and
    ``interference`` the DC offset and hum.
    """
    signal: np.ndarray
    noise: np.ndarray
    interference: np.ndarray
    signal_power: float              # in-band, over the active span
    noise_power: float               # in-band, over the whole output

    @property
    def total(self) -> np.ndarray:
        return self.signal + self.noise + self.interference

    @property
    def snr_db(self) -> float:
        return 10 * np.log10(self.signal_power / self.noise_power) if self.noise_power else np.inf


@dataclass
class FmChannel:
    """The SPEC 14.2 FM audio chain (see the module docstring for every value).

    ``snr_db`` None (or inf) runs the chain without noise. ``dc`` and
    ``hum_db`` None take the profile's defaults; ``hum_db`` -inf disables hum.

    ``cnr_db`` replaces the additive audio noise (stages 4 and 5, and
    ``snr_db``, which is then ignored) by an FM link at that carrier-to-noise
    ratio (``fm_discriminate``, with ``max_dev_hz`` at the path's deviation
    clip and the IF bandwidth ``if_bw_hz``): the noise rises 6 dB/octave as
    in stage 5 above the FM threshold, and turns into clicks below it. Single
    hop profiles only. ``comb`` adds a ``Comb`` of lines to the output, with
    the DC offset and hum.
    """
    profile: str = "mic"
    snr_db: float | None = 25.0
    drive_db: float = -3.0
    sco_ppm: float = 0.0
    dc: float | None = None
    hum_db: float | None = None
    width: int = params.BASE_TONES
    cnr_db: float | None = None
    max_dev_hz: float = FM_MAX_DEV_HZ
    if_bw_hz: float = FM_IF_BW_HZ
    comb: Comb | None = None
    _profile: Profile = field(init=False, repr=False)

    def __post_init__(self) -> None:
        if self.profile not in PROFILES:
            raise ValueError(f"unknown profile {self.profile!r}; choose from {sorted(PROFILES)}")
        self._profile = PROFILES[self.profile]
        if self.cnr_db is not None and self._profile.hops != 1:
            raise ValueError("cnr_db models one FM hop; the repeater profile has two")

    def __call__(self, x: np.ndarray, rng: np.random.Generator | None = None) -> np.ndarray:
        return self.run(x, rng).total

    @property
    def ratio(self) -> float:
        return 1.0 + self.sco_ppm * 1e-6

    def _noisy(self) -> bool:
        if self.cnr_db is not None:
            return True
        return self.snr_db is not None and np.isfinite(self.snr_db)

    def run(self, x: np.ndarray, rng: np.random.Generator | None = None) -> ChannelOutput:
        prof = self._profile
        x = np.asarray(x, dtype=float)
        nz = np.flatnonzero(x)
        if nz.size == 0:
            raise ValueError("input is silent")
        if rng is None:
            if self._noisy():
                raise ValueError("a noisy channel needs an rng")
            rng = np.random.default_rng(0)
        hum_phase = rng.uniform(0, 2 * np.pi)      # drawn first: same hum with or without noise
        x = x * (10 ** (self.drive_db / 20) / np.max(np.abs(x)))
        active = (nz[-1] - nz[0] + 1) / self.ratio
        w = self.width

        def sig_power(s: np.ndarray) -> float:
            return in_band_energy(s, w) / active

        if prof.hops == 1:
            sent = _tx(prof.path, x)
            clean = resample(_rx(prof.path, sent), self.ratio)
            p_s = sig_power(clean)
            noise = np.zeros_like(clean)
            if self.cnr_db is not None:
                full_scale = prof.path.clip if prof.path is not None else 1.0
                heard = fm_discriminate(sent, self.cnr_db, rng, full_scale=full_scale,
                                        max_dev_hz=self.max_dev_hz, if_bw_hz=self.if_bw_hz)
                noise = resample(_rx(prof.path, heard), self.ratio) - clean
            elif self._noisy():
                n = resample(_noise(prof.path, len(x), rng), self.ratio)
                g = np.sqrt(p_s / 10 ** (self.snr_db / 10) / in_band_power(n, w))
                noise = g * n
        else:
            clean, noise, p_s = self._repeater(x, rng, sig_power)

        p_n = in_band_power(noise, w)
        interference = self._interference(len(clean), p_s, hum_phase)
        if self.comb is not None:
            interference = interference + self.comb.wave(len(clean), p_s, rng)
        return ChannelOutput(clean, noise, interference, p_s, p_n)

    def _between_hops(self, x: np.ndarray) -> np.ndarray:
        prof = self._profile
        if prof.controller is not None:
            x = _filt(prof.controller.sos, x)
        if prof.dispersion_s:
            x = _filt(dispersion_sos(prof.dispersion_s), x)
        return x

    def _repeater(self, x, rng, sig_power):
        path, w = self._profile.path, self.width
        hop1 = _rx(path, _tx(path, x))
        ref = resample(_rx(path, _tx(path, self._between_hops(hop1))), self.ratio)
        p_s = sig_power(ref)
        if not self._noisy():
            return ref, np.zeros_like(ref), p_s
        # Hop 1: half the output noise, so SNR + 3 dB at the repeater's receiver.
        target_db = self.snr_db + 10 * np.log10(2)
        n1 = _noise(path, len(x), rng)
        g1 = np.sqrt(sig_power(hop1) / 10 ** (target_db / 10) / in_band_power(n1, w))
        rpt = _rx(path, _tx(path, self._between_hops(hop1 + g1 * n1)))
        resid = resample(rpt, self.ratio) - ref
        n2 = resample(_noise(path, len(x), rng), self.ratio)
        # Hop 2: solve |resid + g n2|^2 = target in band (cross term included).
        (sr, sn), n_fft = _band_spectra(w, resid, n2)
        e_r = 2 * np.sum(np.abs(sr) ** 2) / n_fft
        e_n = 2 * np.sum(np.abs(sn) ** 2) / n_fft
        e_c = 2 * np.real(np.sum(sr * np.conj(sn))) / n_fft
        e_t = p_s / 10 ** (self.snr_db / 10) * len(ref)
        disc = e_c ** 2 - e_n * (e_r - e_t)
        g2 = max((-e_c + np.sqrt(disc)) / e_n, 0.0) if disc >= 0 and e_r < e_t else 0.0
        return ref, resid + g2 * n2, p_s

    def _interference(self, n: int, p_s: float, phase: float) -> np.ndarray:
        prof = self._profile
        dc = prof.dc if self.dc is None else self.dc
        hum_db = prof.hum_db if self.hum_db is None else self.hum_db
        out = np.full(n, float(dc))
        if hum_db is not None and np.isfinite(hum_db):
            amp = np.sqrt(2 * p_s * 10 ** (hum_db / 10))
            out += amp * np.sin(2 * np.pi * HUM_HZ * np.arange(n) / FS + phase)
        return out
