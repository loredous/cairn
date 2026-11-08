//! A frame-level discrete-event "air" for the link layer.
//!
//! Stations are `cairn_link::Station`s. A transmission occupies the air
//! from keying to PTT release (the Appendix A frame model with each
//! station's TXDelay); every station that can hear it sees raw DCD for that
//! interval and, `DECODE_LATENCY` after the last symbol, a decoded frame, a
//! failed header, or nothing. Losses: forced header loss and ACK loss per
//! directed link, per-block loss from a logistic BLER curve around the §6.2
//! thresholds, tone impairments that hurt frames whose mask still uses the
//! tone, collisions where two audible transmissions overlap, half-duplex
//! deafness while transmitting, and a scripted filter. Foreign carriers (a
//! third station keying up) can be injected.

#![allow(dead_code)]

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::time::Duration;

use cairn_link::capacity::{AppendixA, FrameModel, FrameShape};
use cairn_link::params::RUNG_THRESHOLDS_DB;
use cairn_link::rng::Rng;
use cairn_link::{
    Action, Address, Config, Event, HostCommand, HostEvent, Instant, LinkFrame, Measurement, Pdu, RxFrame,
};

/// Processing time from the last symbol to the decoded frame.
pub const DECODE_LATENCY: Duration = Duration::from_millis(30);
/// Slope of the logistic BLER curve (dB).
pub const BLER_SLOPE_DB: f64 = 0.4;
/// SNR penalty on a frame whose applied mask uses an impaired tone.
pub const IMPAIRED_TONE_PENALTY_DB: f64 = 12.0;

/// Block error probability at `snr` for a rung whose 1% point is `thr`.
pub fn bler(snr: f64, thr: f64) -> f64 {
    let thr50 = thr - BLER_SLOPE_DB * 99f64.ln();
    1.0 / (1.0 + ((snr - thr50) / BLER_SLOPE_DB).exp())
}

pub fn t(s: f64) -> Instant {
    Instant::from_secs_f64(s)
}

/// What the scripted filter decides for one reception.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fate {
    /// Use the loss model.
    Normal,
    /// Preamble detected, header failed.
    HeaderLost,
    /// Nothing detected at all.
    Missed,
    /// These control blocks fail.
    DropControls(Vec<usize>),
    /// Every data block fails.
    DropData,
}

/// A scripted reception filter: (sim, receiver, transmission) -> fate.
pub type Filter = Box<dyn FnMut(&Sim, usize, &AirTx) -> Fate>;

/// A transmission on the air.
#[derive(Clone, Debug)]
pub struct AirTx {
    pub from: usize,
    pub start: Instant,
    pub last_symbol: Instant,
    pub end: Instant,
    pub frame: Option<LinkFrame>,
    pub turn_holder: bool,
    pub heard_by: Vec<usize>,
}

/// One directed link.
#[derive(Clone, Debug)]
pub struct Link {
    pub audible: bool,
    pub snr_db: f64,
    pub header_loss: f64,
    pub ack_loss: f64,
    /// Extra per-block loss probability on top of the BLER curve.
    pub block_loss: f64,
    /// Tones that arrive this many dB below the rest.
    pub impaired: Vec<(u8, f64)>,
}

impl Default for Link {
    fn default() -> Self {
        Link { audible: true, snr_db: 25.0, header_loss: 0.0, ack_loss: 0.0, block_loss: 0.0, impaired: vec![] }
    }
}

#[derive(Debug)]
enum Ev {
    Wake { st: usize, generation: u64 },
    Host { st: usize, cmd: HostCommand },
    TxEnd { st: usize, air: usize },
    Rx { st: usize, air: usize },
    Foreign { start: bool, air: usize },
    Snr { from: usize, to: usize, snr: f64 },
    Audible { from: usize, to: usize, on: bool },
    Raw { st: usize, ev: Event },
    Impair { from: usize, to: usize, tones: Vec<(u8, f64)> },
}

pub struct SimStation {
    pub st: cairn_link::Station,
    pub model: AppendixA,
    pub wake_gen: u64,
    pub transmitting: Option<usize>,
    pub carriers: usize,
    pub dcd_reported: bool,
    pub delivered: Vec<(Instant, Address, Pdu)>,
    pub broadcasts: Vec<(Instant, Address, Pdu)>,
    pub events: Vec<(Instant, HostEvent)>,
    pub sent: Vec<usize>,
    /// Reconnect (resume) this long after a link loss or failed resume, as a host would.
    pub auto_resume: Option<Duration>,
    pub peer: Option<Address>,
    /// Every decoded session DATA frame: (time, [(sequence, block ok)]).
    pub rx_data: Vec<(Instant, Vec<(cairn_link::Seq, bool)>)>,
    /// PDUs the host queues once the first session comes up.
    pub on_connect: Vec<(u64, Pdu)>,
}

pub struct Sim {
    pub now: Instant,
    pub stations: Vec<SimStation>,
    pub links: Vec<Vec<Link>>,
    pub air: Vec<AirTx>,
    queue: BinaryHeap<Reverse<(Instant, u64, usize)>>,
    evs: Vec<Option<Ev>>,
    seq: u64,
    pub rng: Rng,
    pub meas_noise_db: f64,
    pub csma_violations: Vec<String>,
    pub turn_holder_overlaps: Vec<String>,
    pub mask_mismatches: usize,
    pub invalid_frames: Vec<String>,
    pub filter: Option<Filter>,
    pub foreign_audible: Vec<bool>,
    pub trace: bool,
    pending_unhear: Vec<(usize, usize)>,
}

impl Sim {
    pub fn new(seed: u64) -> Sim {
        Sim {
            now: Instant::ZERO,
            stations: Vec::new(),
            links: Vec::new(),
            air: Vec::new(),
            queue: BinaryHeap::new(),
            evs: Vec::new(),
            seq: 0,
            rng: Rng::new(seed ^ 0xA1A1),
            meas_noise_db: 0.25,
            csma_violations: Vec::new(),
            turn_holder_overlaps: Vec::new(),
            mask_mismatches: 0,
            invalid_frames: Vec::new(),
            filter: None,
            foreign_audible: Vec::new(),
            trace: std::env::var_os("SIM_TRACE").is_some(),
            pending_unhear: Vec::new(),
        }
    }

    /// Add a station with its TXDelay; links to every other station default
    /// to audible at 25 dB.
    pub fn add(&mut self, cfg: Config, txdelay: Duration) -> usize {
        let model = AppendixA::with_txdelay(txdelay);
        let st = cairn_link::Station::new(cfg, Box::new(model));
        self.stations.push(SimStation {
            st,
            model,
            wake_gen: 0,
            transmitting: None,
            carriers: 0,
            dcd_reported: false,
            delivered: vec![],
            broadcasts: vec![],
            events: vec![],
            sent: vec![],
            auto_resume: None,
            peer: None,
            rx_data: vec![],
            on_connect: vec![],
        });
        let n = self.stations.len();
        for row in &mut self.links {
            row.push(Link::default());
        }
        self.links.push(vec![Link::default(); n]);
        self.foreign_audible.push(true);
        n - 1
    }

    pub fn addr(&self, st: usize) -> Address {
        self.stations[st].st.address()
    }

    pub fn link(&mut self, from: usize, to: usize) -> &mut Link {
        &mut self.links[from][to]
    }

    /// Both directions' SNR.
    pub fn set_snr(&mut self, a: usize, b: usize, snr: f64) {
        self.links[a][b].snr_db = snr;
        self.links[b][a].snr_db = snr;
    }

    fn push(&mut self, at: Instant, ev: Ev) {
        self.seq += 1;
        self.evs.push(Some(ev));
        self.queue.push(Reverse((at, self.seq, self.evs.len() - 1)));
    }

    pub fn host(&mut self, st: usize, at: Instant, cmd: HostCommand) {
        if let HostCommand::Connect { peer, .. } = &cmd {
            self.stations[st].peer = Some(*peer);
        }
        self.push(at, Ev::Host { st, cmd });
    }

    /// Schedule an SNR step on one directed link.
    pub fn snr_step(&mut self, at: Instant, from: usize, to: usize, snr: f64) {
        self.push(at, Ev::Snr { from, to, snr });
    }

    /// Deliver an arbitrary event to a station at a time.
    pub fn event(&mut self, st: usize, at: Instant, ev: Event) {
        self.push(at, Ev::Raw { st, ev });
    }

    /// Add a station with an explicit frame model (repeater access delay).
    pub fn add_with_model(&mut self, cfg: Config, model: AppendixA) -> usize {
        let i = self.add(cfg, model.txdelay);
        self.stations[i].model = model;
        self.stations[i].st = cairn_link::Station::new(self.stations[i].st.config().clone(), Box::new(model));
        i
    }

    /// Schedule a change of audibility, both directions.
    pub fn outage(&mut self, a: usize, b: usize, from: Instant, until: Instant) {
        for (x, y) in [(a, b), (b, a)] {
            self.push(from, Ev::Audible { from: x, to: y, on: false });
            self.push(until, Ev::Audible { from: x, to: y, on: true });
        }
    }

    /// Schedule a tone impairment on one directed link.
    pub fn impair(&mut self, at: Instant, from: usize, to: usize, tones: Vec<(u8, f64)>) {
        self.push(at, Ev::Impair { from, to, tones });
    }

    /// A third station keys up for `dur` at `at` (heard by stations whose
    /// `foreign_audible` flag is set).
    pub fn foreign(&mut self, at: Instant, dur: Duration) {
        self.air.push(AirTx {
            from: usize::MAX,
            start: at,
            last_symbol: at + dur,
            end: at + dur,
            frame: None,
            turn_holder: false,
            heard_by: vec![],
        });
        let i = self.air.len() - 1;
        self.push(at, Ev::Foreign { start: true, air: i });
        self.push(at + dur, Ev::Foreign { start: false, air: i });
    }

    fn audible(&self, from: usize, to: usize) -> bool {
        if from == usize::MAX { self.foreign_audible[to] } else { from != to && self.links[from][to].audible }
    }

    fn update_dcd(&mut self, st: usize) {
        let s = &self.stations[st];
        if s.transmitting.is_some() {
            return;
        }
        let up = s.carriers > 0;
        if up != s.dcd_reported {
            self.stations[st].dcd_reported = up;
            self.deliver(st, Event::Dcd(up));
        }
    }

    pub fn deliver(&mut self, st: usize, ev: Event) {
        let now = self.now;
        if let Event::Frame(f) = &ev {
            if f.header.ftype == cairn_link::FrameType::Data {
                if let Some(seqs) = f.block_seqs() {
                    let v = seqs.into_iter().zip(f.data.iter().map(|d| d.is_some())).collect();
                    self.stations[st].rx_data.push((now, v));
                }
            }
        }
        if self.trace {
            match &ev {
                Event::Frame(f) => eprintln!("{now} [{st}] rx {:?} seq {}", f.header.ftype, f.header.seq),
                Event::Dcd(_) | Event::Tick | Event::Host(HostCommand::Send { .. }) => {}
                e => eprintln!("{now} [{st}] {e:?}"),
            }
        }
        let actions = self.stations[st].st.handle(ev, now);
        for a in actions {
            match a {
                Action::Transmit { frame, turn_holder } => self.transmit(st, frame, turn_holder),
                Action::Wakeup(at) => {
                    self.stations[st].wake_gen += 1;
                    let generation = self.stations[st].wake_gen;
                    self.push(at.max(now), Ev::Wake { st, generation });
                }
                Action::Deliver { from, pdu } => self.stations[st].delivered.push((now, from, pdu)),
                Action::DeliverBroadcast { from, pdu, .. } => self.stations[st].broadcasts.push((now, from, pdu)),
                Action::Host(e) => {
                    if self.trace {
                        eprintln!("{now} [{st}] host {e:?}");
                    }
                    if let (Some(d), HostEvent::LinkLost { peer }) = (self.stations[st].auto_resume, &e) {
                        let cmd = HostCommand::Connect { peer: *peer, new_session: false };
                        self.push(now + d, Ev::Host { st, cmd });
                    }
                    if let (Some(d), HostEvent::ConnectFailed { peer, reason }) = (self.stations[st].auto_resume, &e) {
                        if *reason == cairn_link::ConnectFailure::NoAnswer && self.stations[st].st.has_retained(*peer) {
                            let cmd = HostCommand::Connect { peer: *peer, new_session: false };
                            self.push(now + d, Ev::Host { st, cmd });
                        }
                    }
                    if matches!(e, HostEvent::Connected { .. }) {
                        for (id, pdu) in std::mem::take(&mut self.stations[st].on_connect) {
                            self.push(now, Ev::Host { st, cmd: HostCommand::Send { id, pdu } });
                        }
                    }
                    self.stations[st].events.push((now, e));
                }
            }
        }
    }

    fn shape(f: &LinkFrame) -> FrameShape {
        FrameShape {
            mcs: f.mcs,
            width: f.tx.width,
            mask: f.tx.mask,
            controls: f.controls.len(),
            data: f.data.len(),
            probe: f.flags.probe,
            long_prefix: f.tx.long_prefix,
        }
    }

    fn transmit(&mut self, st: usize, frame: LinkFrame, turn_holder: bool) {
        let now = self.now;
        if let Err(e) = frame.validate() {
            self.invalid_frames.push(format!("{now} [{st}] {e:?} {frame:?}"));
        }
        if self.stations[st].transmitting.is_some() {
            self.invalid_frames.push(format!("{now} [{st}] keyed while transmitting"));
        }
        // Keying while a carrier this station can hear is on the air.
        for a in self.air.iter().rev().take(64) {
            if a.from != st && a.start < now && now < a.end && self.audible(a.from, st) {
                let msg = format!(
                    "{now} [{st}] keyed {:?} over station {} (turn_holder {turn_holder})",
                    frame.ftype, a.from as isize
                );
                if turn_holder { self.turn_holder_overlaps.push(msg) } else { self.csma_violations.push(msg) }
            }
        }
        if self.trace {
            eprintln!(
                "{now} [{st}] TX {:?} seq {} mcs {} ctl {} data {} th {turn_holder}",
                frame.ftype,
                frame.seq,
                frame.mcs,
                frame.controls.len(),
                frame.data.len()
            );
        }
        let m = self.stations[st].model;
        let shape = Self::shape(&frame);
        let dur = m.on_air_time(&shape);
        let last_symbol = now + m.time_to_last_symbol(&shape);
        let heard_by: Vec<usize> = (0..self.stations.len()).filter(|&r| self.audible(st, r)).collect();
        self.air.push(AirTx {
            from: st,
            start: now,
            last_symbol,
            end: now + dur,
            frame: Some(frame),
            turn_holder,
            heard_by: heard_by.clone(),
        });
        let i = self.air.len() - 1;
        self.stations[st].transmitting = Some(i);
        self.stations[st].sent.push(i);
        for r in heard_by {
            {
                self.stations[r].carriers += 1;
                self.update_dcd(r);
                self.push(last_symbol + DECODE_LATENCY, Ev::Rx { st: r, air: i });
            }
        }
        self.push(now + dur, Ev::TxEnd { st, air: i });
    }

    fn reception(&mut self, r: usize, i: usize) -> Option<Event> {
        let a = self.air[i].clone();
        let frame = a.frame.clone()?;
        if !self.links[a.from][r].audible {
            return None; // the link dropped during the frame
        }
        // Deaf while transmitting during the frame.
        let deaf = self.stations[r].sent.iter().rev().take(8).any(|&j| {
            let b = &self.air[j];
            b.start < a.last_symbol && a.start < b.end
        });
        if deaf {
            return None;
        }
        let collided = self.air.iter().enumerate().rev().take(64).any(|(j, b)| {
            j != i && b.from != r && self.audible(b.from, r) && b.start < a.last_symbol && a.start < b.end
        });
        if collided {
            return Some(Event::HeaderFailed);
        }
        let mut fate = Fate::Normal;
        if let Some(mut f) = self.filter.take() {
            fate = f(self, r, &a);
            self.filter = Some(f);
        }
        let link = self.links[a.from][r].clone();
        match fate {
            Fate::Missed => return None,
            Fate::HeaderLost => return Some(Event::HeaderFailed),
            _ => {}
        }
        let snr = link.snr_db;
        if self.rng.chance(link.header_loss) || self.rng.chance(bler(snr, RUNG_THRESHOLDS_DB[0])) {
            return Some(Event::HeaderFailed);
        }
        let impaired_used = link.impaired.iter().any(|(t, _)| frame.tx.mask >> t & 1 == 1 && *t < frame.tx.width);
        let eff = if impaired_used { snr - IMPAIRED_TONE_PENALTY_DB } else { snr };
        let mut rx = RxFrame::perfect(&frame, Measurement::default(), a.last_symbol);
        // Mask by ID: the receiver demaps with the mask the header names.
        let mask_ok = match self.stations[r].st.rx_mask(&rx.header) {
            None => false,
            Some(m) => {
                let w = (1u64 << frame.tx.width.min(56)) - 1;
                if (m & w) != (frame.tx.mask & w) {
                    self.mask_mismatches += 1;
                    false
                } else {
                    true
                }
            }
        };
        for (k, c) in rx.controls.iter_mut().enumerate() {
            let ack_lost =
                k == 0 && matches!(c, Some(cairn_link::ControlBlock::Ack(_))) && self.rng.chance(link.ack_loss);
            let dropped = matches!(&fate, Fate::DropControls(v) if v.contains(&k));
            if !mask_ok || ack_lost || dropped || self.rng.chance(bler(eff, RUNG_THRESHOLDS_DB[0])) {
                *c = None;
            }
        }
        let thr = RUNG_THRESHOLDS_DB[frame.mcs as usize];
        for d in rx.data.iter_mut() {
            if !mask_ok || fate == Fate::DropData || self.rng.chance(link.block_loss) || self.rng.chance(bler(eff, thr))
            {
                *d = None;
            }
        }
        let noise = (self.rng.unit() - 0.5) * 2.0 * self.meas_noise_db;
        let mut tones = vec![snr + noise; 24];
        for (t, depth) in &link.impaired {
            if (*t as usize) < tones.len() {
                tones[*t as usize] -= depth;
            }
        }
        rx.meas = Measurement {
            snr_db: snr + noise,
            distortion: 0,
            adc_clipped: false,
            tone_snr_db: Some(tones),
            probe: frame.flags.probe,
        };
        Some(Event::Frame(rx))
    }

    fn step(&mut self) -> bool {
        let Some(Reverse((at, _, idx))) = self.queue.pop() else { return false };
        self.now = self.now.max(at);
        let ev = self.evs[idx].take().expect("event");
        match ev {
            Ev::Wake { st, generation } => {
                if generation == self.stations[st].wake_gen {
                    self.deliver(st, Event::Tick);
                }
            }
            Ev::Host { st, cmd } => self.deliver(st, Event::Host(cmd)),
            Ev::Snr { from, to, snr } => self.links[from][to].snr_db = snr,
            Ev::Impair { from, to, tones } => self.links[from][to].impaired = tones,
            Ev::Raw { st, ev } => self.deliver(st, ev),
            Ev::Audible { from, to, on } => {
                // A carrier already on the air keeps its effect until it ends.
                self.links[from][to].audible = on;
                if !on {
                    if let Some(i) = self.stations[from].transmitting {
                        let _ = i;
                        self.stations[to].carriers = self.stations[to].carriers.saturating_sub(1);
                        self.update_dcd(to);
                        self.pending_unhear.push((from, to));
                    }
                }
            }
            Ev::TxEnd { st, air } => {
                self.stations[st].transmitting = None;
                let heard: Vec<usize> = self.air[air].heard_by.clone();
                for r in heard {
                    if !self.pending_unhear.contains(&(st, r)) {
                        self.stations[r].carriers -= 1;
                        self.update_dcd(r);
                    }
                }
                self.pending_unhear.retain(|&(f, _)| f != st);
                self.deliver(st, Event::TxDone);
                self.update_dcd(st);
            }
            Ev::Rx { st, air } => {
                if let Some(e) = self.reception(st, air) {
                    self.deliver(st, e);
                }
            }
            Ev::Foreign { start, .. } => {
                for r in 0..self.stations.len() {
                    if self.foreign_audible[r] {
                        if start {
                            self.stations[r].carriers += 1
                        } else {
                            self.stations[r].carriers -= 1
                        }
                        self.update_dcd(r);
                    }
                }
            }
        }
        true
    }

    /// Run until `end` (simulated time).
    pub fn run_until(&mut self, end: Instant) {
        while let Some(Reverse((at, _, _))) = self.queue.peek() {
            if *at > end {
                break;
            }
            self.step();
        }
        self.now = self.now.max(end);
    }

    /// Run until `cond` holds or `end`.
    pub fn run_while(&mut self, end: Instant, mut cond: impl FnMut(&Sim) -> bool) -> bool {
        while let Some(Reverse((at, _, _))) = self.queue.peek() {
            if *at > end {
                break;
            }
            self.step();
            if !cond(self) {
                return true;
            }
        }
        false
    }

    /// Every delivered session byte at a station, in order.
    pub fn delivered_bytes(&self, st: usize) -> Vec<u8> {
        self.stations[st].delivered.iter().flat_map(|(_, _, p)| p.body.clone()).collect()
    }

    /// Transmissions by a station.
    pub fn frames_from(&self, st: usize) -> Vec<&AirTx> {
        self.stations[st].sent.iter().map(|&i| &self.air[i]).collect()
    }

    /// Overlapping transmissions between two stations.
    pub fn collisions_between(&self, a: usize, b: usize) -> usize {
        let fa = self.frames_from(a);
        let fb = self.frames_from(b);
        fa.iter().map(|x| fb.iter().filter(|y| x.start < y.end && y.start < x.end).count()).sum()
    }

    /// Blocks a station received intact more than once.
    pub fn duplicate_blocks(&self, st: usize) -> usize {
        let mut seen = std::collections::HashSet::new();
        let mut dup = 0;
        for (_, v) in &self.stations[st].rx_data {
            for (s, ok) in v {
                if *ok && !seen.insert(s.0) {
                    dup += 1;
                }
            }
        }
        dup
    }

    pub fn has_event(&self, st: usize, f: impl Fn(&HostEvent) -> bool) -> bool {
        self.stations[st].events.iter().any(|(_, e)| f(e))
    }

    pub fn event_time(&self, st: usize, f: impl Fn(&HostEvent) -> bool) -> Option<Instant> {
        self.stations[st].events.iter().find(|(_, e)| f(e)).map(|(t, _)| *t)
    }
}

/// A station config with a fictitious callsign.
pub fn cfg(call: &str, seed: u64) -> Config {
    let mut c = Config::for_callsign(call, 0);
    c.seed = seed;
    c
}

/// Two stations, A (index 0) and B (index 1), at `snr` both ways.
pub fn pair(seed: u64, snr: f64) -> Sim {
    pair_with(seed, snr, |_| {})
}

/// Two stations with a config tweak applied to both.
pub fn pair_with(seed: u64, snr: f64, tweak: impl Fn(&mut Config)) -> Sim {
    let mut sim = Sim::new(seed);
    let mut a = cfg("N0AAA", seed.wrapping_mul(31).wrapping_add(1));
    let mut b = cfg("N0BBB", seed.wrapping_mul(37).wrapping_add(2));
    tweak(&mut a);
    tweak(&mut b);
    sim.add(a, Duration::from_millis(150));
    sim.add(b, Duration::from_millis(150));
    sim.set_snr(0, 1, snr);
    sim
}

/// Proptest case count: `PROPTEST_CASES` if set, else `default`.
pub fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Assert the run broke no channel-access or framing rule.
pub fn assert_clean(sim: &Sim) {
    assert!(sim.invalid_frames.is_empty(), "{:?}", sim.invalid_frames);
    assert!(sim.csma_violations.is_empty(), "{:?}", sim.csma_violations);
    assert!(sim.turn_holder_overlaps.is_empty(), "{:?}", sim.turn_holder_overlaps);
    assert_eq!(sim.mask_mismatches, 0);
}

/// Deterministic pseudo-random payload.
pub fn payload(n: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng::new(seed);
    (0..n).map(|_| r.next_u64() as u8).collect()
}

/// Queue `data` as PDUs of at most `chunk` bytes on station `st` once its
/// session comes up.
pub fn send_on_connect(sim: &mut Sim, st: usize, data: &[u8], chunk: usize) {
    for (i, c) in data.chunks(chunk).enumerate() {
        sim.stations[st].on_connect.push((i as u64, Pdu::message(c.to_vec())));
    }
}

/// Queue `data` as PDUs of at most `chunk` bytes on station `st`.
pub fn send_chunks(sim: &mut Sim, st: usize, at: Instant, data: &[u8], chunk: usize) -> usize {
    let mut n = 0;
    for (i, c) in data.chunks(chunk).enumerate() {
        sim.host(st, at, HostCommand::Send { id: i as u64, pdu: Pdu::message(c.to_vec()) });
        n += 1;
    }
    n
}
