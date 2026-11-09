//! Two (or more) complete modem runtimes on a simulated audio channel, on a
//! virtual clock faster than real time.
//!
//! Every station is a real [`Modem`]: real PHY transmit and receive, the
//! real link layer, real carrier detect. The medium sums the audio of every
//! keyed transmitter each receiver can hear, resampled for the two
//! stations' sample-clock offset, and adds that receiver's noise. The noise
//! models FM quieting: with no carrier on the channel the receiver hears
//! open-squelch noise (as loud in the signal band as the reference signal,
//! so 0 dB "SNR"); while any carrier it hears is keyed (from PTT on, so
//! during TXDelay too) the noise drops to the level of that link's SNR
//! (the strongest carrier wins). SNR is the 7 dB-equivalent in-band SNR of
//! SPEC 7.1 and `channel.awgn` (noise power in 300 Hz to the 24-tone band
//! edge against the power a frame clipped at 7 dB has). The noise is white,
//! band-limited 150 Hz to 4 kHz like a receiver's audio, so the quieting
//! band above the highest tone sees it. Outages silence a pair of stations
//! to each other (no carrier, no audio); foreign transmitters (a Bell 202
//! packet station) can be injected.
//!
//! Frequency-selective links: [`Sim::set_shape`] puts a linear-phase FIR on
//! the audio one station hears from another (the radios' audio response),
//! and [`SimStation::noise_shape`] colours a receiver's noise; [`radio_like`]
//! gives both for the radios of the first on-air session (2026-11-07: tones
//! 0-2 at 350-550 Hz far down, the response rolling off above 4.2 kHz).
//! A station's [`SimStation::radio_tail`] keeps its carrier on the air that
//! long after PTT release, while its own receiver hears silence (those
//! radios: about 0.65 s).
//!
//! Time is the medium's sample clock; each station processes 10 ms chunks,
//! wakes at the times it asks for, and its transmissions start at the time
//! it keys (sample-exact), with its own TXDelay and TXTail.

use std::collections::BTreeMap;
use std::time::Duration;

use cairn_link::{Address, FrameType, HostCommand, HostEvent, Instant, LinkFrame, Pdu};
use cairn_phy::channel::Rng;
use cairn_phy::params::{BAND_LOW_HZ, FS, band_high_hz};

use crate::runtime::{FrameInfo, Modem, ModemConfig, Output};

/// Samples per chunk (10 ms).
pub const CHUNK: usize = 480;

/// Time of medium sample `n`.
pub fn t_of(n: usize) -> Instant {
    Instant((n as u128 * 1_000_000 / FS as u128) as u64)
}

/// Medium sample at time `t` (rounded up).
pub fn n_of(t: Instant) -> usize {
    ((t.0 as u128 * FS as u128).div_ceil(1_000_000)) as usize
}

/// A second-order section (direct form II transposed).
#[derive(Clone, Copy, Debug)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn lowpass(f0: f64, q: f64) -> Self {
        let w = 2.0 * std::f64::consts::PI * f0 / FS as f64;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b = [(1.0 - c) / 2.0 / a0, (1.0 - c) / a0, (1.0 - c) / 2.0 / a0];
        Biquad { b, a: [-2.0 * c / a0, (1.0 - alpha) / a0], z: [0.0; 2] }
    }

    fn highpass(f0: f64, q: f64) -> Self {
        let w = 2.0 * std::f64::consts::PI * f0 / FS as f64;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b = [(1.0 + c) / 2.0 / a0, -(1.0 + c) / a0, (1.0 + c) / 2.0 / a0];
        Biquad { b, a: [-2.0 * c / a0, (1.0 - alpha) / a0], z: [0.0; 2] }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// White Gaussian noise band-limited to a receiver's audio band
/// (Butterworth: 2nd-order high-pass at 150 Hz, 4th-order low-pass at
/// 4 kHz), flat within 0.5 dB from 300 Hz to 3.1 kHz, so its density there
/// is that of the unit-variance white noise it is made from.
#[derive(Clone, Debug)]
pub struct ReceiverNoise {
    rng: Rng,
    f: [Biquad; 3],
}

impl ReceiverNoise {
    /// Seeded.
    pub fn new(seed: u64) -> Self {
        ReceiverNoise {
            rng: Rng::new(seed),
            f: [
                Biquad::highpass(150.0, std::f64::consts::FRAC_1_SQRT_2),
                Biquad::lowpass(4000.0, 0.541_196_1),
                Biquad::lowpass(4000.0, 1.306_563),
            ],
        }
    }

    /// The next sample.
    pub fn sample(&mut self) -> f64 {
        let mut v = self.rng.normal();
        for f in &mut self.f {
            v = f.run(v);
        }
        v
    }
}

/// A linear-phase FIR (odd length) with the magnitude response given by
/// `points` (Hz, dB; linear in dB between points, flat beyond the ends),
/// by frequency sampling on a 1 Hz grid and a Hann window.
pub fn fir_from_response(points: &[(f64, f64)], taps: usize) -> Vec<f64> {
    assert!(taps % 2 == 1 && !points.is_empty(), "odd length, some points");
    let gain = |f: f64| -> f64 {
        let db = if f <= points[0].0 {
            points[0].1
        } else if f >= points[points.len() - 1].0 {
            points[points.len() - 1].1
        } else {
            let i = points.windows(2).position(|w| f >= w[0].0 && f <= w[1].0).expect("bracket");
            let (a, b) = (points[i], points[i + 1]);
            a.1 + (b.1 - a.1) * (f - a.0) / (b.0 - a.0)
        };
        10f64.powf(db / 20.0)
    };
    let m = (taps / 2) as f64;
    let nyq = FS as f64 / 2.0;
    let grid = 2400usize;
    let mut h = vec![0.0; taps];
    for (n, v) in h.iter_mut().enumerate() {
        let t = n as f64 - m;
        let mut acc = 0.0;
        for k in 0..=grid {
            let f = nyq * k as f64 / grid as f64;
            let w = if k == 0 || k == grid { 0.5 } else { 1.0 };
            acc += w * gain(f) * (2.0 * std::f64::consts::PI * f * t / FS as f64).cos();
        }
        let window = 0.5 + 0.5 * (std::f64::consts::PI * t / (m + 1.0)).cos();
        *v = acc / grid as f64 * window;
    }
    h
}

/// |H(f)| of an FIR, dB.
pub fn fir_gain_db(h: &[f64], f_hz: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * f_hz / FS as f64;
    let (re, im) = h
        .iter()
        .enumerate()
        .fold((0.0, 0.0), |(re, im), (n, &v)| (re + v * (w * n as f64).cos(), im - v * (w * n as f64).sin()));
    10.0 * (re * re + im * im).max(1e-30).log10()
}

/// The audio response (signal FIR) and noise colouring of the radios of the
/// first on-air session (2026-11-07, two handhelds through AIOC cables):
/// per-tone SNR measured there was -13 to +7 dB on tones 0-2 (350-550 Hz)
/// against 9-28 dB on tones 3-31, rising with frequency, with the passband
/// to about tone 39 (4.25 kHz); the reported SNR at 32 tones was 13 dB
/// unmasked and 21-25 dB with tones 0-2 masked, so the low tones carry far
/// more noise, not just less signal. Here the receiver's noise is 25 dB
/// stronger at 300-350 Hz (from 0 dB at 150 Hz), 17 at 450, 11 at 550, 4
/// at 650 and 2 at 750 Hz,
/// then tilts from +1.5 dB at 1.1 kHz to -2.5 dB at 3.5 kHz; the signal is
/// 3 dB down at 350 Hz, flat from 650 Hz to 3.9 kHz, and rolls off to
/// -10 dB at 4.4 kHz and -30 dB at 5 kHz. Per tone that is the link SNR
/// less about 28, 18, 11, 4 and 2 dB on tones 0-4, then -1.5 to +2.5 dB.
pub fn radio_like() -> (Vec<f64>, Vec<f64>) {
    let signal = fir_from_response(
        &[
            (0.0, -20.0),
            (250.0, -12.0),
            (350.0, -3.0),
            (450.0, -1.5),
            (550.0, -0.5),
            (650.0, 0.0),
            (3900.0, 0.0),
            (4200.0, -3.0),
            (4400.0, -10.0),
            (5000.0, -30.0),
        ],
        1023,
    );
    let noise = fir_from_response(
        &[
            (0.0, 0.0),
            (150.0, 0.0),
            (300.0, 25.0),
            (350.0, 25.0),
            (450.0, 17.0),
            (550.0, 11.0),
            (650.0, 4.0),
            (750.0, 2.0),
            (900.0, 1.5),
            (1100.0, 1.5),
            (3500.0, -2.5),
        ],
        1023,
    );
    (signal, noise)
}

/// A streaming FIR.
#[derive(Clone, Debug)]
pub struct Fir {
    h: Vec<f64>,
    buf: Vec<f64>,
    pos: usize,
}

impl Fir {
    /// From taps.
    pub fn new(h: Vec<f64>) -> Self {
        let n = h.len();
        Fir { h, buf: vec![0.0; n], pos: 0 }
    }

    /// The next output.
    pub fn run(&mut self, x: f64) -> f64 {
        let n = self.h.len();
        self.buf[self.pos] = x;
        let mut acc = 0.0;
        for (k, &c) in self.h.iter().enumerate() {
            acc += c * self.buf[(self.pos + n - k) % n];
        }
        self.pos = (self.pos + 1) % n;
        acc
    }
}

/// `x` through the FIR `h` (full length: the tail is kept).
pub fn convolve(x: &[f64], h: &[f64]) -> Vec<f64> {
    let mut y = vec![0.0; x.len() + h.len() - 1];
    for (i, &v) in x.iter().enumerate() {
        if v != 0.0 {
            for (k, &c) in h.iter().enumerate() {
                y[i + k] += v * c;
            }
        }
    }
    y
}

/// Mean power of a frame clipped at 7 dB at the transmitter's output scale
/// (peaks at -3 dBFS): the reference of the 7 dB-equivalent SNR.
pub fn reference_power() -> f64 {
    use cairn_phy::{FrameConfig, Transmitter};
    let cfg = FrameConfig::new(24, 0, 7).expect("config");
    let f = crate::frames::build_frame(&mut Transmitter::new(), &cfg, 1);
    let y = f.output();
    y.iter().map(|v| v * v).sum::<f64>() / y.len() as f64
}

/// White-noise variance for an in-band SNR at 24 tones against `p_ref`.
pub fn noise_variance(p_ref: f64, snr_db: f64) -> f64 {
    p_ref / 10f64.powf(snr_db / 10.0) * (FS as f64 / 2.0) / (band_high_hz(24) - BAND_LOW_HZ)
}

/// Cubic (Catmull-Rom) resampling: `y[m] = x(m * ratio)`.
pub fn resample(x: &[f64], ratio: f64) -> Vec<f64> {
    if (ratio - 1.0).abs() < 1e-12 {
        return x.to_vec();
    }
    let n = ((x.len() as f64 - 1.0) / ratio).floor().max(0.0) as usize;
    let at = |i: isize| -> f64 { if i < 0 || i as usize >= x.len() { 0.0 } else { x[i as usize] } };
    (0..n)
        .map(|m| {
            let p = m as f64 * ratio;
            let i = p.floor() as isize;
            let f = p - i as f64;
            let (p0, p1, p2, p3) = (at(i - 1), at(i), at(i + 1), at(i + 2));
            p1 + 0.5 * f * (p2 - p0 + f * (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3 + f * (3.0 * (p1 - p2) + p3 - p0)))
        })
        .collect()
}

/// Who transmitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Station `i`.
    Station(usize),
    /// A foreign transmitter (heard by every station at its SNR).
    Foreign,
}

/// One transmission on the medium.
#[derive(Clone, Debug)]
pub struct AirTx {
    /// The transmitter.
    pub from: Source,
    /// PTT on (medium sample).
    pub key: usize,
    /// First audio sample.
    pub audio_start: usize,
    /// One past the last audio sample.
    pub audio_end: usize,
    /// PTT off.
    pub release: usize,
    /// The carrier's end: the release plus the transmitter's radio tail.
    pub carrier_end: usize,
    /// The audio at the transmitter's clock.
    pub audio: Vec<f64>,
    /// The link frame (stations).
    pub frame: Option<LinkFrame>,
    /// A turn-holder reply.
    pub turn_holder: bool,
    /// SNR at every receiver (foreign transmitters).
    pub foreign_snr_db: f64,
    /// Resampled audio per receiver.
    rx_audio: BTreeMap<usize, Vec<f64>>,
}

impl AirTx {
    /// Keyed at `t`.
    pub fn key_time(&self) -> Instant {
        t_of(self.key)
    }
    /// Last audio sample's end.
    pub fn end_time(&self) -> Instant {
        t_of(self.audio_end)
    }
}

/// A station in the simulation.
pub struct SimStation {
    /// The modem.
    pub modem: Modem,
    /// TXDelay.
    pub txdelay: Duration,
    /// TXTail.
    pub txtail: Duration,
    /// Sample-clock offset, ppm.
    pub ppm: f64,
    noise: ReceiverNoise,
    /// Host events.
    pub events: Vec<(Instant, HostEvent)>,
    /// Session PDUs delivered.
    pub delivered: Vec<(Instant, Address, Pdu)>,
    /// Broadcast PDUs delivered (UI and BEACON).
    pub broadcasts: Vec<(Instant, Address, FrameType, Pdu)>,
    /// Decoded headers.
    pub heard: Vec<FrameInfo>,
    /// Log lines.
    pub log: Vec<(Instant, String)>,
    /// Carrier-detect changes (raw, no hold), each detector's state.
    pub dcd: Vec<(Instant, cairn_phy::dcd::DcdState)>,
    /// Transmission in progress: (air index, release time).
    tx: Option<(usize, Instant)>,
    /// Commands to issue.
    pending: Vec<(Instant, HostCommand)>,
    /// Reconnect this long after a link loss (resume), when set.
    pub auto_resume: Option<Duration>,
    /// The radio keeps its carrier this long after PTT release (no audio);
    /// meanwhile its own receiver hears silence.
    pub radio_tail: Duration,
    /// Colouring of this receiver's noise (FIR taps), when set.
    pub noise_shape: Option<Vec<f64>>,
    /// Receive level: everything this station hears is scaled by it (1.0:
    /// a frame peaks at -3 dBFS, as transmitted).
    pub rx_gain: f64,
    noise_fir: Option<Fir>,
    /// When set, every output is also kept here (transmit audio dropped),
    /// with PTT releases as `None`, for a host driving the station through
    /// the native API (`hostapi`); the host takes them.
    pub outputs: Option<Vec<(Instant, Option<Output>)>>,
}

impl SimStation {
    /// Bytes of every delivered MESSAGE and FILE-CHUNK PDU, in order (the
    /// FILE-CHUNK object and offset prefix removed).
    pub fn delivered_bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        for (_, _, p) in &self.delivered {
            if p.pdu_type == cairn_link::pdu::pdu_type::FILE_CHUNK && p.body.len() >= 8 {
                v.extend_from_slice(&p.body[8..]);
            } else {
                v.extend_from_slice(&p.body);
            }
        }
        v
    }

    /// Whether a host event happened.
    pub fn has(&self, f: impl Fn(&HostEvent) -> bool) -> bool {
        self.events.iter().any(|(_, e)| f(e))
    }

    /// When a host event first happened.
    pub fn when(&self, f: impl Fn(&HostEvent) -> bool) -> Option<Instant> {
        self.events.iter().find(|(_, e)| f(e)).map(|(t, _)| *t)
    }
}

/// The simulation.
pub struct Sim {
    /// Stations.
    pub stations: Vec<SimStation>,
    /// Everything transmitted.
    pub air: Vec<AirTx>,
    /// SNR from station i to j.
    pub snr: Vec<Vec<f64>>,
    /// Audio response from station i to j (FIR taps), when set.
    shapes: BTreeMap<(usize, usize), Vec<f64>>,
    /// Scheduled response changes: (time, from, to, taps or none).
    shape_steps: Vec<(Instant, usize, usize, Option<Vec<f64>>)>,
    /// Scheduled SNR changes: (time, from, to, snr).
    snr_steps: Vec<(Instant, usize, usize, f64)>,
    /// Outages between pairs: (a, b, from, until).
    pub outages: Vec<(usize, usize, Instant, Instant)>,
    /// Open-squelch noise level in the band, dB relative to the reference
    /// signal power (0: as loud as a frame clipped at 7 dB).
    pub open_squelch_db: f64,
    p_ref: f64,
    /// Medium position: every station has heard up to here.
    pub pos: usize,
    /// Print a trace.
    pub trace: bool,
}

impl Sim {
    /// An empty medium.
    pub fn new() -> Self {
        Sim {
            stations: Vec::new(),
            air: Vec::new(),
            snr: Vec::new(),
            shapes: BTreeMap::new(),
            shape_steps: Vec::new(),
            snr_steps: Vec::new(),
            outages: Vec::new(),
            open_squelch_db: 0.0,
            p_ref: reference_power(),
            pos: 0,
            trace: std::env::var_os("SIM_TRACE").is_some(),
        }
    }

    /// Add a station; every link to and from it starts at `snr_db`.
    pub fn add(&mut self, cfg: ModemConfig, ppm: f64, snr_db: f64, seed: u64) -> usize {
        let i = self.stations.len();
        let (txdelay, txtail) = (cfg.txdelay, cfg.txtail);
        self.stations.push(SimStation {
            modem: Modem::new(cfg),
            txdelay,
            txtail,
            ppm,
            noise: ReceiverNoise::new(seed ^ 0x5eed),
            events: Vec::new(),
            delivered: Vec::new(),
            broadcasts: Vec::new(),
            heard: Vec::new(),
            log: Vec::new(),
            dcd: Vec::new(),
            tx: None,
            pending: Vec::new(),
            auto_resume: None,
            radio_tail: Duration::ZERO,
            noise_shape: None,
            rx_gain: 1.0,
            noise_fir: None,
            outputs: None,
        });
        for row in &mut self.snr {
            row.push(snr_db);
        }
        self.snr.push(vec![snr_db; i + 1]);
        i
    }

    /// The address of station `i`.
    pub fn addr(&self, i: usize) -> Address {
        self.stations[i].modem.station().address()
    }

    /// Set both directions' SNR between two stations.
    pub fn set_snr(&mut self, a: usize, b: usize, snr: f64) {
        self.snr[a][b] = snr;
        self.snr[b][a] = snr;
    }

    /// The audio station `to` hears from `from` goes through this FIR.
    pub fn set_shape(&mut self, from: usize, to: usize, taps: Vec<f64>) {
        self.shapes.insert((from, to), taps);
    }

    /// Change the response from `from` to `to` at a time (`None`: flat);
    /// transmissions keyed from then on take it.
    pub fn shape_step(&mut self, at: Instant, from: usize, to: usize, taps: Option<Vec<f64>>) {
        self.shape_steps.push((at, from, to, taps));
    }

    /// Both directions between two stations take the radio-like response
    /// and both receivers the radio-like noise colouring ([`radio_like`]),
    /// received 12 dB below the transmitted level (frames peak near
    /// -15 dBFS; the open-squelch noise, with its strong low end, would
    /// clip otherwise).
    pub fn radio_like_link(&mut self, a: usize, b: usize) {
        let (signal, noise) = radio_like();
        self.set_shape(a, b, signal.clone());
        self.set_shape(b, a, signal);
        for i in [a, b] {
            self.stations[i].noise_shape = Some(noise.clone());
            self.stations[i].rx_gain = 10f64.powf(-12.0 / 20.0);
        }
    }

    /// Change one direction's SNR at a time.
    pub fn snr_step(&mut self, at: Instant, from: usize, to: usize, snr: f64) {
        self.snr_steps.push((at, from, to, snr));
    }

    /// Stations `a` and `b` cannot hear each other in this interval.
    pub fn outage(&mut self, a: usize, b: usize, from: Instant, until: Instant) {
        self.outages.push((a, b, from, until));
    }

    /// Call into station `i`'s modem now (a host request), then route what
    /// it said, so a transmission it asks for keys at this time.
    pub fn modem_call<R>(&mut self, i: usize, f: impl FnOnce(&mut Modem, Instant) -> R) -> R {
        let now = self.now();
        let r = f(&mut self.stations[i].modem, now);
        self.route(i, now);
        r
    }

    /// Issue a host command at a time.
    pub fn host(&mut self, st: usize, at: Instant, cmd: HostCommand) {
        self.stations[st].pending.push((at, cmd));
        self.stations[st].pending.sort_by_key(|(t, _)| *t);
    }

    /// A foreign transmitter: PTT at `at`, `audio` after `lead`, PTT off
    /// `tail` after it, heard by every station at `snr_db`.
    pub fn foreign(&mut self, at: Instant, lead: Duration, audio: Vec<f64>, tail: Duration, snr_db: f64) {
        let key = n_of(at);
        let audio_start = key + (lead.as_secs_f64() * FS as f64).round() as usize;
        let audio_end = audio_start + audio.len();
        let release = audio_end + (tail.as_secs_f64() * FS as f64).round() as usize;
        self.air.push(AirTx {
            from: Source::Foreign,
            key,
            audio_start,
            audio_end,
            release,
            carrier_end: release,
            audio,
            frame: None,
            turn_holder: false,
            foreign_snr_db: snr_db,
            rx_audio: BTreeMap::new(),
        });
    }

    fn hears(&self, from: Source, to: usize, n: usize) -> bool {
        match from {
            Source::Foreign => true,
            Source::Station(f) => {
                if f == to {
                    return false;
                }
                let t = t_of(n);
                !self
                    .outages
                    .iter()
                    .any(|&(a, b, s, e)| ((a == f && b == to) || (a == to && b == f)) && t >= s && t < e)
            }
        }
    }

    fn link_snr(&self, from: Source, to: usize, tx: &AirTx) -> f64 {
        match from {
            Source::Foreign => tx.foreign_snr_db,
            Source::Station(f) => self.snr[f][to],
        }
    }

    /// The input of station `r` for medium samples `n0..n1`.
    fn input(&mut self, r: usize, n0: usize, n1: usize) -> Vec<f64> {
        let len = n1 - n0;
        let mut x = vec![0.0; len];
        let open = noise_variance(self.p_ref, -self.open_squelch_db).sqrt();
        let mut sigma = vec![open; len];
        let ppm_r = self.stations[r].ppm;
        let mut own_tail = vec![false; len];
        for k in 0..self.air.len() {
            let (from, key, release, audio_start) =
                (self.air[k].from, self.air[k].key, self.air[k].carrier_end, self.air[k].audio_start);
            if release <= n0 || key >= n1 {
                continue;
            }
            if from == Source::Station(r) {
                // Own radio still transmitting after PTT release: its
                // receiver hears (nearly) nothing.
                let ptt_off = self.air[k].release;
                for (i, n) in (n0..n1).enumerate() {
                    if n >= ptt_off && n < release {
                        own_tail[i] = true;
                    }
                }
                continue;
            }
            let snr = self.link_snr(from, r, &self.air[k]);
            let s_link = noise_variance(self.p_ref, snr).sqrt();
            // The receiver's view of this transmitter's audio, resampled for
            // the relative sample-clock offset.
            if !self.air[k].rx_audio.contains_key(&r) {
                let ppm_t = match from {
                    Source::Station(f) => self.stations[f].ppm,
                    Source::Foreign => 0.0,
                };
                let ratio = (1.0 + ppm_t * 1e-6) / (1.0 + ppm_r * 1e-6);
                let mut y = resample(&self.air[k].audio, ratio);
                if let Source::Station(f) = from {
                    if let Some(h) = self.shapes.get(&(f, r)) {
                        // Zero phase: the linear-phase FIR's delay taken
                        // out, so the audio keeps its place against the
                        // carrier (the filter's delay is longer than TXTail).
                        let m = h.len() / 2;
                        y = convolve(&y, h)[m..m + y.len()].to_vec();
                    }
                }
                self.air[k].rx_audio.insert(r, y);
            }
            for (i, n) in (n0..n1).enumerate() {
                if n < key || n >= release || !self.hears(from, r, n) {
                    continue;
                }
                sigma[i] = sigma[i].min(s_link);
                if n >= audio_start {
                    if let Some(v) = self.air[k].rx_audio[&r].get(n - audio_start) {
                        x[i] += v;
                    }
                }
            }
        }
        let st = &mut self.stations[r];
        if st.noise_fir.is_none() {
            st.noise_fir = st.noise_shape.clone().map(Fir::new);
        }
        for ((v, s), tail) in x.iter_mut().zip(&sigma).zip(&own_tail) {
            let mut n = st.noise.sample();
            if let Some(f) = st.noise_fir.as_mut() {
                n = f.run(n);
            }
            *v = st.rx_gain * if *tail { 0.003 * open * n } else { *v + s * n };
        }
        x
    }

    fn route(&mut self, st: usize, now: Instant) {
        for o in self.stations[st].modem.drain() {
            if let Some(v) = self.stations[st].outputs.as_mut() {
                let kept = match &o {
                    Output::Transmit(req) => {
                        Output::Transmit(crate::runtime::TxRequest { audio: Vec::new(), ..req.clone() })
                    }
                    other => other.clone(),
                };
                v.push((now, Some(kept)));
            }
            match o {
                Output::Transmit(req) => {
                    let s = &self.stations[st];
                    let key = n_of(req.key_at);
                    let audio_start = key + (s.txdelay.as_secs_f64() * FS as f64).round() as usize;
                    let audio: Vec<f64> = req.audio.iter().map(|&v| v as f64).collect();
                    let audio_end = audio_start + audio.len();
                    let release = audio_end + (s.txtail.as_secs_f64() * FS as f64).round() as usize;
                    let carrier_end = release + (s.radio_tail.as_secs_f64() * FS as f64).round() as usize;
                    if self.trace {
                        eprintln!(
                            "{} [{st}] TX {:?} seq {} MCS {} ctl {} data {} th {} ({:.3} s)",
                            req.key_at,
                            req.frame.ftype,
                            req.frame.seq.0,
                            req.frame.mcs,
                            req.frame.controls.len(),
                            req.frame.data.len(),
                            req.turn_holder,
                            (release - key) as f64 / FS as f64
                        );
                    }
                    self.air.push(AirTx {
                        from: Source::Station(st),
                        key,
                        audio_start,
                        audio_end,
                        release,
                        carrier_end,
                        audio,
                        frame: Some(req.frame),
                        turn_holder: req.turn_holder,
                        foreign_snr_db: 0.0,
                        rx_audio: BTreeMap::new(),
                    });
                    self.stations[st].tx = Some((self.air.len() - 1, t_of(release)));
                }
                Output::Host(e) => {
                    if self.trace {
                        eprintln!("{now} [{st}] host {e:?}");
                    }
                    if let (Some(d), HostEvent::LinkLost { peer }) = (self.stations[st].auto_resume, &e) {
                        let cmd = HostCommand::Connect { peer: *peer, new_session: false };
                        self.host(st, now + d, cmd);
                    }
                    if let (Some(d), HostEvent::ConnectFailed { peer, .. }) = (self.stations[st].auto_resume, &e) {
                        if self.stations[st].modem.station().has_retained(*peer) {
                            let cmd = HostCommand::Connect { peer: *peer, new_session: false };
                            self.host(st, now + d, cmd);
                        }
                    }
                    self.stations[st].events.push((now, e));
                }
                Output::Deliver { from, pdu, .. } => self.stations[st].delivered.push((now, from, pdu)),
                Output::DeliverBroadcast { from, ftype, pdu, .. } => {
                    self.stations[st].broadcasts.push((now, from, ftype, pdu))
                }
                Output::Heard(f) => {
                    if self.trace {
                        eprintln!("{} [{st}] rx {}", f.ended_at, f.line());
                    }
                    self.stations[st].heard.push(f);
                }
                Output::HeaderFailed { at, .. } => {
                    if self.trace {
                        eprintln!("{at} [{st}] header failed");
                    }
                }
                Output::Dcd { at, state } => {
                    if self.trace {
                        eprintln!("{at} [{st}] dcd {state:?}");
                    }
                    self.stations[st].dcd.push((at, state));
                }
                Output::Log(l) => {
                    if self.trace {
                        eprintln!("{now} [{st}] {l}");
                    }
                    self.stations[st].log.push((now, l))
                }
                Output::Discarded { from, error, .. } => {
                    self.stations[st].log.push((now, format!("PDU from {from} discarded: {error}")))
                }
            }
        }
    }

    /// Run until `end`.
    pub fn run_until(&mut self, end: Instant) {
        self.run_while(end, |_| true);
    }

    /// Run until `end` or until `cond` is false (checked every chunk).
    /// Returns whether `cond` still held at the end.
    pub fn run_while(&mut self, end: Instant, mut cond: impl FnMut(&Sim) -> bool) -> bool {
        loop {
            if !cond(self) {
                return false;
            }
            let chunk_end = self.pos + CHUNK;
            let t_chunk = t_of(chunk_end);
            if t_of(self.pos) >= end {
                return true;
            }
            // Events before the chunk boundary, in time order.
            loop {
                let mut best: Option<(Instant, usize, u8)> = None;
                for (i, s) in self.stations.iter().enumerate() {
                    let mut cand = Vec::new();
                    if let Some((_, rel)) = s.tx {
                        cand.push((rel, 0u8));
                    }
                    if let Some((t, _)) = s.pending.first() {
                        cand.push((*t, 1));
                    }
                    if let Some(w) = s.modem.next_wakeup() {
                        cand.push((w, 2));
                    }
                    for (t, kind) in cand {
                        if t < t_chunk && best.is_none_or(|(bt, _, _)| t < bt) {
                            best = Some((t, i, kind));
                        }
                    }
                }
                for &(t, from, to, snr) in &self.snr_steps.clone() {
                    if t < t_chunk && t >= t_of(self.pos) {
                        self.snr[from][to] = snr;
                    }
                }
                for (t, from, to, taps) in std::mem::take(&mut self.shape_steps) {
                    if t < t_chunk {
                        match taps {
                            Some(h) => self.shapes.insert((from, to), h),
                            None => self.shapes.remove(&(from, to)),
                        };
                    } else {
                        self.shape_steps.push((t, from, to, taps));
                    }
                }
                self.snr_steps.retain(|(t, ..)| *t >= t_chunk || *t < t_of(self.pos));
                let Some((t, i, kind)) = best else { break };
                let t = t.max(t_of(self.pos));
                match kind {
                    0 => {
                        let (_, rel) = self.stations[i].tx.take().expect("tx");
                        self.stations[i].modem.tx_done(rel, t);
                        if let Some(v) = self.stations[i].outputs.as_mut() {
                            v.push((t, None));
                        }
                    }
                    1 => {
                        let (_, cmd) = self.stations[i].pending.remove(0);
                        self.stations[i].modem.host(cmd, t);
                    }
                    _ => self.stations[i].modem.tick(t),
                }
                self.route(i, t);
            }
            for i in 0..self.stations.len() {
                let x = self.input(i, self.pos, chunk_end);
                self.stations[i].modem.push_audio(&x, t_of(self.pos), t_chunk);
                self.route(i, t_chunk);
            }
            self.pos = chunk_end;
            // Keep the audio of old transmissions out of memory.
            for a in &mut self.air {
                if a.carrier_end + FS as usize <= self.pos {
                    a.audio = Vec::new();
                    a.rx_audio.clear();
                }
            }
        }
    }

    /// Current time.
    pub fn now(&self) -> Instant {
        t_of(self.pos)
    }

    /// Transmissions of a station.
    pub fn frames_from(&self, st: usize) -> Vec<&AirTx> {
        self.air.iter().filter(|a| a.from == Source::Station(st)).collect()
    }

    /// Turn-holder reply times: from the end of the peer's last audio
    /// sample to keying, for replies within 1 s of a peer frame.
    pub fn turnarounds(&self) -> Vec<Duration> {
        let mut v = Vec::new();
        for a in self.air.iter().filter(|a| a.turn_holder) {
            let Source::Station(me) = a.from else { continue };
            let prev = self
                .air
                .iter()
                .filter(|b| b.from != Source::Station(me) && b.from != Source::Foreign && b.audio_end <= a.key)
                .max_by_key(|b| b.audio_end);
            if let Some(p) = prev {
                let d = a.key_time().saturating_since(p.end_time());
                if d < Duration::from_secs(1) {
                    v.push(d);
                }
            }
        }
        v
    }

    /// Any transmission keyed while another station it could hear was on
    /// the air (from PTT on to the end of its carrier, radio tail included),
    /// other than at the same instant.
    pub fn overlaps(&self) -> Vec<(usize, usize)> {
        let mut v = Vec::new();
        for (i, a) in self.air.iter().enumerate() {
            for (j, b) in self.air.iter().enumerate() {
                if i != j && a.key > b.key && a.key < b.carrier_end {
                    if let Source::Station(me) = a.from {
                        if self.hears(b.from, me, a.key) {
                            v.push((i, j));
                        }
                    }
                }
            }
        }
        v
    }
}

impl Default for Sim {
    fn default() -> Self {
        Self::new()
    }
}

/// Percentile of durations, ms.
pub fn percentile_ms(v: &[Duration], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s: Vec<f64> = v.iter().map(|d| d.as_secs_f64() * 1e3).collect();
    s.sort_by(f64::total_cmp);
    let i = ((p / 100.0) * (s.len() - 1) as f64).round() as usize;
    s[i.min(s.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_is_flat_in_band() {
        // In-band power of the band-limited noise equals white noise's.
        let mut n = ReceiverNoise::new(1);
        let x: Vec<f64> = (0..FS as usize * 4).map(|_| n.sample()).collect();
        let p: f64 = x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64;
        // 150-4000 Hz of 24 kHz: about 0.16.
        assert!((0.14..0.18).contains(&p), "{p}");
    }

    #[test]
    fn resample_keeps_a_tone() {
        let x: Vec<f64> =
            (0..4800).map(|i| (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48_000.0).sin()).collect();
        let y = resample(&x, 1.0 + 100e-6);
        let err: f64 = y
            .iter()
            .enumerate()
            .skip(10)
            .take(4000)
            .map(|(m, v)| {
                let t = m as f64 * (1.0 + 100e-6);
                (v - (2.0 * std::f64::consts::PI * 1000.0 * t / 48_000.0).sin()).abs()
            })
            .fold(0.0, f64::max);
        assert!(err < 1e-4, "{err}");
    }

    #[test]
    fn radio_like_response() {
        let (s, n) = radio_like();
        let tone = |k: usize| 350.0 + 100.0 * k as f64;
        // Per-tone SNR offsets against the link SNR.
        for (k, want) in [(0, -28.0), (1, -18.5), (2, -11.5), (3, -4.0), (4, -2.0), (10, -1.0), (31, 2.5)] {
            let g = fir_gain_db(&s, tone(k)) - fir_gain_db(&n, tone(k));
            assert!((g - want).abs() < 1.5, "tone {k}: {g:.1} dB, want {want}");
        }
        assert!(fir_gain_db(&s, 4400.0) < -8.0 && fir_gain_db(&s, 5000.0) < -25.0);
        // Streaming and block convolution agree.
        let x: Vec<f64> = (0..600).map(|i| ((i * 37) % 11) as f64 - 5.0).collect();
        let y = convolve(&x, &s);
        let mut f = Fir::new(s.clone());
        for (i, &v) in x.iter().enumerate() {
            assert!((f.run(v) - y[i]).abs() < 1e-9);
        }
    }

    #[test]
    fn sample_times_round_trip() {
        for n in [0usize, 1, 479, 480, 48_000, 123_456_789] {
            assert!(n_of(t_of(n)) == n || n_of(t_of(n)) == n + 1, "{n}");
        }
    }
}
