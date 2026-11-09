"""FM channel model (SPEC 14.2): each stage against its analytic expectation."""

from __future__ import annotations

import numpy as np
import pytest
from scipy import signal

from cairn_sim import channel, frame, params

FS = params.FS


def tone(f_hz: float, seconds: float = 1.0, phase: float = 0.0) -> np.ndarray:
    return np.cos(2 * np.pi * f_hz * np.arange(int(seconds * FS)) / FS + phase)


def amplitude_at(x: np.ndarray, f_hz: float) -> float:
    """Amplitude of the component at ``f_hz`` (integer cycles in ``x`` assumed)."""
    n = np.arange(x.size)
    return 2 * abs(np.mean(x * np.exp(-2j * np.pi * f_hz * n / FS)))


def response_db(sos: np.ndarray, f_hz) -> np.ndarray:
    _, h = signal.sosfreqz(sos, worN=np.atleast_1d(np.asarray(f_hz, dtype=float)), fs=FS)
    return 20 * np.log10(np.abs(h))


def quiet(profile: str, **kw) -> channel.FmChannel:
    """Noise-free channel without DC or hum."""
    return channel.FmChannel(profile, snr_db=None, dc=0.0, hum_db=-np.inf, **kw)


# --- limiter and deviation clip ------------------------------------------------


def test_soft_limiter_is_linear_to_the_knee_then_saturates():
    x = np.linspace(-4, 4, 80_001)
    y = channel.soft_limit(x, knee=1.0, ceiling=1.413)
    below = np.abs(x) <= 1.0
    assert np.array_equal(y[below], x[below])
    assert np.all(np.diff(y) >= 0)                          # monotonic
    assert np.max(np.abs(y)) < 1.413
    slope = np.gradient(y, x)
    knee = np.argmin(np.abs(x - 1.0))
    assert abs(slope[knee + 1] - 1) < 1e-3                   # no kink at the knee
    # Output in dB versus input in dB: 0.5 dB/dB or less a little above the knee.
    db_slope = slope * x / np.where(y == 0, 1, y)
    assert db_slope[np.argmin(np.abs(x - 1.5))] < 0.5


def test_hard_clip_level():
    x = np.linspace(-3, 3, 6001)
    y = channel.hard_clip(x, 1.259)
    assert np.max(y) == pytest.approx(1.259) and np.min(y) == pytest.approx(-1.259)
    inside = np.abs(x) <= 1.259
    assert np.array_equal(y[inside], x[inside])


def test_mic_chain_knee_at_unit_drive_of_a_1khz_tone():
    """Stepped 1 kHz tone (SPEC 13.5): 1 dB/dB below the knee, compressed above."""
    x = tone(1000.0, 0.5)
    drives = np.arange(-9.0, 10.0, 3.0)
    out_db = np.array([20 * np.log10(amplitude_at(quiet("mic", drive_db=d)(x)[4800:], 1000.0))
                       for d in drives])
    step = np.diff(out_db) / 3.0
    assert np.allclose(out_db[drives < 0], drives[drives < 0], atol=0.3)  # unity gain at 1 kHz
    assert np.all(np.abs(step[:3] - 1) < 0.02)               # -9 .. 0 dB: linear
    assert np.all(step[4:] < 0.5)                            # +3 dB and up: limited


def test_flat_port_clips_hard_at_maximum_deviation():
    x = tone(1000.0, 0.5)
    lin = quiet("flat25", drive_db=-3.0)(x)
    assert 20 * np.log10(amplitude_at(lin[4800:], 1000.0)) == pytest.approx(-3.0, abs=0.05)
    hot = quiet("flat25", drive_db=6.0)(x)
    # A tone clipped at 1.0 has a fundamental of at most 4/pi.
    assert amplitude_at(hot[4800:], 1000.0) < 4 / np.pi * 1.01


# --- emphasis, tilt, filters ---------------------------------------------------


def test_preemphasis_slope_and_net_emphasis_flat():
    f = np.linspace(300, 3000, 271)
    net = response_db(channel.preemphasis_sos(), f) + response_db(channel.deemphasis_sos(), f)
    assert np.max(np.abs(net)) < 1.0
    # Analytic 750 us network (1 + j f/212.2) / (1 + j f/8000), unity at 1 kHz.
    fz = 1 / (2 * np.pi * 750e-6)

    def analog(f):
        return 20 * np.log10(np.abs((1 + 1j * f / fz) / (1 + 1j * f / 8000.0)))

    pre = response_db(channel.preemphasis_sos(), f)
    assert np.allclose(pre, analog(f) - analog(1000.0), atol=0.25)   # bilinear warping
    octave = response_db(channel.preemphasis_sos(), [1000.0, 2000.0])
    assert 5.5 < octave[1] - octave[0] < 6.02                 # ~6 dB/octave
    assert octave[0] == pytest.approx(0.0, abs=1e-9)          # unity at 1 kHz


def test_noise_tilt_rises_6db_per_octave():
    f = np.array([400.0, 800.0, 1600.0, 3200.0])
    r = response_db(channel.noise_tilt_sos(), f)
    assert np.allclose(np.diff(r), 20 * np.log10(2), atol=0.15)


def _noise_psd(profile: str, f_hz: list[float]) -> np.ndarray:
    rng = np.random.default_rng(11)
    x = tone(1000.0, 4.0)
    out = channel.FmChannel(profile, snr_db=20.0, dc=0.0, hum_db=-np.inf).run(x, rng)
    f, p = signal.welch(out.noise, FS, nperseg=4800)
    return 10 * np.log10(np.interp(f_hz, f, p))


def test_flat_port_noise_tilt_reaches_the_output():
    p = _noise_psd("flat25", [500.0, 2000.0])
    assert p[1] - p[0] == pytest.approx(12.0, abs=1.0)       # two octaves at 6 dB


def test_mic_path_noise_is_flattened_by_de_emphasis():
    p = _noise_psd("mic", [500.0, 2000.0])
    assert abs(p[1] - p[0]) < 1.5


def _bands():
    seen = []
    for prof in channel.PROFILES.values():
        path = prof.path
        for b in (path.tx_band, path.rx_band) if path else ():
            if b is not None and b not in seen:
                seen.append(b)
        if prof.controller is not None and prof.controller not in seen:
            seen.append(prof.controller)
    return seen


@pytest.mark.parametrize("band", _bands(), ids=lambda b: f"{b.lo_hz}-{b.hi_hz}")
def test_band_pass_edges_at_minus_3db(band):
    corners = [c for c in (band.lo_hz, band.hi_hz) if c is not None]
    assert np.allclose(response_db(band.sos, corners), -3.01, atol=0.05)
    mid = np.sqrt(band.lo_hz * band.hi_hz)
    assert abs(response_db(band.sos, mid)[0]) < 0.1


def test_dispersion_group_delay_spread_is_2ms():
    sos = channel.dispersion_sos(2e-3)
    f = np.linspace(300, 2700, 2401)
    gd = channel.group_delay_s(sos, f)
    assert np.ptp(gd) == pytest.approx(2e-3, abs=2e-6)
    assert np.allclose(response_db(sos, f), 0.0, atol=1e-9)  # all-pass


# --- sample-clock offset --------------------------------------------------------


def _frequency(y: np.ndarray) -> float:
    z = signal.hilbert(y)
    n = np.arange(y.size)
    keep = slice(y.size // 10, -y.size // 10)
    slope = np.polyfit(n[keep], np.unwrap(np.angle(z[keep])), 1)[0]
    return slope * FS / (2 * np.pi)


@pytest.mark.parametrize("ppm", [100.0, -100.0, 1000.0])
def test_resampler_scales_tone_frequency_by_one_plus_eps(ppm):
    f0 = 2000.0
    x = tone(f0, 3.0, phase=0.4)
    r = 1 + ppm * 1e-6
    y = channel.resample(x, r)
    assert y.size == int(np.floor((x.size - 1) / r)) + 1
    assert _frequency(y) == pytest.approx(f0 * r, abs=1e-4)
    ref = np.cos(2 * np.pi * f0 * r * np.arange(y.size) / FS + 0.4)
    mid = slice(200, -200)
    err = np.mean((y[mid] - ref[mid]) ** 2) / np.mean(ref[mid] ** 2)
    assert 10 * np.log10(err) < -85


def test_channel_applies_sample_clock_offset():
    x = tone(1500.0, 2.0)
    y = quiet("flat25", sco_ppm=100.0)(x)
    assert _frequency(y) == pytest.approx(1500.0 * (1 + 1e-4), abs=1e-3)


# --- levels, DC and hum ---------------------------------------------------------


def test_drive_sets_peak_and_ideal_is_untouched():
    rng = np.random.default_rng(12)
    x = rng.standard_normal(4800)
    y = quiet("ideal", drive_db=-3.0)(x)
    assert np.max(np.abs(y)) == pytest.approx(10 ** (-3 / 20))
    assert np.allclose(y, x * 10 ** (-3 / 20) / np.max(np.abs(x)))


def test_dc_offset_and_hum_levels():
    x = tone(1000.0, 2.0)
    out = channel.FmChannel("mic", snr_db=None, dc=0.02, hum_db=-20.0).run(x, np.random.default_rng(13))
    assert np.mean(out.interference) == pytest.approx(0.02, abs=1e-4)
    hum = amplitude_at(out.interference, 60.0)
    assert 10 * np.log10(hum ** 2 / 2 / out.signal_power) == pytest.approx(-20.0, abs=0.05)


# --- audio SNR -------------------------------------------------------------------


@pytest.fixture(scope="module")
def frame_samples() -> np.ndarray:
    return frame.build(frame.FrameConfig(mcs=2, n_blocks=4), np.random.default_rng(14)).samples


@pytest.mark.parametrize("snr_db", [10.0, 30.0])
@pytest.mark.parametrize("profile", sorted(channel.PROFILES))
def test_output_audio_snr_matches_request(frame_samples, profile, snr_db):
    """Measured independently: noisy output minus the noise-free output."""
    kw = {"profile": profile, "sco_ppm": 100.0}
    y = channel.FmChannel(snr_db=snr_db, **kw)(frame_samples, np.random.default_rng(15))
    clean = channel.FmChannel(snr_db=None, **kw)(frame_samples, np.random.default_rng(15))
    p_sig = channel.in_band_power(clean, 24)
    p_noise = channel.in_band_power(y - clean, 24)
    assert 10 * np.log10(p_sig / p_noise) == pytest.approx(snr_db, abs=0.2)


def test_awgn_still_matches_ideal_profile_definition():
    rng = np.random.default_rng(16)
    x = frame.build(frame.FrameConfig(mcs=0, n_blocks=2), rng).samples
    noise = channel.awgn(x, 15.0, 24, rng) - x
    snr = 10 * np.log10(channel.in_band_power(x, 24) / channel.in_band_power(noise, 24))
    assert snr == pytest.approx(15.0, abs=0.2)


# --- integration -----------------------------------------------------------------


@pytest.mark.parametrize("profile,snr_db", [("mic", 15.0), ("flat25", 20.0)])
def test_qpsk_frame_decodes_through_fm_chain(profile, snr_db):
    rng = np.random.default_rng(17)
    cfg = frame.FrameConfig(mcs=0, n_blocks=4)
    tx = frame.build(cfg, rng)
    x = np.concatenate([np.zeros(500), tx.samples, np.zeros(1000)])
    y = channel.FmChannel(profile, snr_db=snr_db)(x, rng)
    r = frame.receive(y, cfg, reference=tx.preamble_samples)
    assert r.header_ok and np.array_equal(r.header_bits, tx.header_bits)
    assert r.block_ok.all() and np.array_equal(r.block_bits, tx.block_bits)


# --- weak-signal impairments -------------------------------------------------------


def test_comb_lines_sit_at_their_levels_against_the_signal():
    x = frame.build(frame.FrameConfig(mcs=2, n_blocks=8), np.random.default_rng(18)).samples
    comb = channel.Comb((-20.0, -15.0, -18.0))
    out = channel.FmChannel("ideal", snr_db=None, comb=comb).run(x, np.random.default_rng(19))
    n = (out.interference.size // 4800) * 4800          # whole periods of every line
    for k, lv in enumerate(comb.levels_dbc, start=1):
        a = amplitude_at(out.interference[:n], 1000.0 * k)
        assert 10 * np.log10(a ** 2 / 2 / out.signal_power) == pytest.approx(lv, abs=0.05)
    assert comb.total_dbc == pytest.approx(10 * np.log10(10 ** -2 + 10 ** -1.5 + 10 ** -1.8))
    assert comb.scaled(3.0).levels_dbc == (-17.0, -12.0, -15.0)


def test_fm_discriminator_is_exact_without_noise():
    a = 0.8 * tone(1000.0, 0.2)
    y = channel.fm_discriminate(a, 200.0, np.random.default_rng(20), full_scale=1.0, max_dev_hz=5000.0)
    assert np.allclose(y, a, atol=1e-6)


def test_fm_noise_rises_6db_per_octave_above_threshold_and_clicks_below():
    """Unmodulated carrier: above threshold the discriminator noise is
    Gaussian with PSD ~ f^2 (+6 dB from 1 to 2 kHz); far below it, clicks
    (heavy-tailed, kurtosis well above 3)."""
    silent = np.zeros(int(2 * FS))
    above = channel.fm_discriminate(silent, 25.0, np.random.default_rng(21))
    f, p = signal.welch(above, FS, nperseg=4800)
    ratio = 10 * np.log10(np.mean(p[(f > 1900) & (f < 2100)]) / np.mean(p[(f > 900) & (f < 1100)]))
    assert ratio == pytest.approx(6.0, abs=0.7)

    def kurtosis(v):
        b = signal.sosfiltfilt(signal.butter(6, [300, 3000], "bandpass", fs=FS, output="sos"), v)
        return np.mean(b ** 4) / np.mean(b ** 2) ** 2
    assert kurtosis(above) == pytest.approx(3.0, abs=0.3)
    assert kurtosis(channel.fm_discriminate(silent, 5.0, np.random.default_rng(22))) > 10.0


def test_fm_channel_snr_follows_cnr_above_threshold_and_falls_off_below():
    """The FM improvement: above threshold the audio SNR is the CNR plus a
    constant (wide FM, 5 kHz deviation, 16 kHz IF); below it, the audio SNR
    falls much faster than the CNR."""
    x = frame.build(frame.FrameConfig(mcs=2, n_blocks=4), np.random.default_rng(23)).samples
    x = np.concatenate([np.zeros(2000), x, np.zeros(2000)])
    snr = {c: channel.FmChannel("mic_wide", cnr_db=c, width=24).run(x, np.random.default_rng(24)).snr_db
           for c in (4.0, 8.0, 15.0, 25.0)}
    assert snr[25.0] - snr[15.0] == pytest.approx(10.0, abs=0.5)
    assert snr[8.0] - snr[4.0] > 8.0


def test_cnr_needs_one_hop():
    with pytest.raises(ValueError):
        channel.FmChannel("repeater", cnr_db=10.0)
