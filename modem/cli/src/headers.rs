//! Frames with real SPEC 8.2 headers for `cairn-modem tx`, and the text
//! `cairn-modem listen` prints for headers and control blocks (Appendix B).

use cairn_phy::fields::{AckBlock, Address, ControlBlock, Header, IdBlock, frame_type, snr_code};
use cairn_phy::params::{self, CP_SHORT};
use cairn_phy::{FrameConfig, LinkState, Transmitter, TxFrame};

use crate::nprng::Pcg64;

/// What a frame with a real header carries.
#[derive(Clone, Debug, PartialEq)]
pub struct HeaderSpec {
    /// Source address (callsign and SSID).
    pub source: Address,
    /// The source callsign as text, for the ID block (B.6).
    pub callsign: String,
    /// Destination (`Address::Broadcast` for a broadcast).
    pub destination: Address,
    /// Frame type (`frame_type`).
    pub frame_type: u8,
    /// Sequence field: the frame counter (broadcast) or the first data
    /// block's sequence number (session).
    pub sequence: u16,
    /// Width, tones: the broadcast width, or the session link's.
    pub width: usize,
    /// Session frames: this station's cap in tones, the probe's extent (None: the width).
    pub cap: Option<usize>,
    /// Session frames: usable tones (None: all).
    pub usable: Option<Vec<usize>>,
    /// Prefix, samples.
    pub cp: usize,
    /// Data rung.
    pub mcs: u8,
    /// Data blocks.
    pub n_data: usize,
    /// Send the ID control block (B.6).
    pub id: bool,
    /// Grid locator for the ID block.
    pub grid: Option<String>,
    /// Payload bytes (zero-padded over the blocks), or None for the
    /// simulator's random payloads for the seed.
    pub message: Option<Vec<u8>>,
}

impl HeaderSpec {
    /// A frame from `source` to everyone at `width` tones (no mask, cap = width).
    pub fn broadcast(source: Address, frame_type: u8, sequence: u16, width: usize, mcs: u8, n_data: usize) -> Self {
        let callsign = match &source {
            Address::Callsign { text, .. } => text.clone(),
            _ => String::new(),
        };
        HeaderSpec {
            source,
            callsign,
            destination: Address::Broadcast,
            frame_type,
            sequence,
            width,
            cap: None,
            usable: None,
            cp: CP_SHORT,
            mcs,
            n_data,
            id: frame_type == frame_type::BEACON,
            grid: None,
            message: None,
        }
    }

    /// A session frame (SPEC 8.2 flag bit 3): DATA, ACK, SOUND, DISCONNECT.
    pub fn is_session(&self) -> bool {
        matches!(
            self.frame_type,
            frame_type::DATA | frame_type::ACK | frame_type::SOUND | frame_type::DISCONNECT | frame_type::CONNECT_ACK
        )
    }

    /// The control blocks this frame carries: in a session the ACK block
    /// first (SPEC 8.4; demonstration values), then the ID block if asked
    /// (always in a BEACON).
    pub fn controls(&self) -> Result<Vec<ControlBlock>, String> {
        let mut c = Vec::new();
        if self.is_session() {
            c.push(ControlBlock::Ack(AckBlock {
                base_sequence: self.sequence.wrapping_sub(1),
                bitmap: 1,
                requested_mcs: self.mcs,
                snr_q: snr_code(20.0),
                ..Default::default()
            }));
        }
        if self.id || self.frame_type == frame_type::BEACON {
            if self.callsign.is_empty() {
                return Err("the ID block needs the callsign as text".into());
            }
            let ssid = match self.source {
                Address::Callsign { ssid, .. } | Address::Hashed { ssid, .. } => ssid,
                Address::Broadcast => 0,
            };
            c.push(ControlBlock::Id(IdBlock { callsign: self.callsign.clone(), ssid, grid: self.grid.clone() }));
        }
        Ok(c)
    }

    /// The header (flag bit 4 is set by the builder when the data needs the probe).
    pub fn header(&self, n_control: usize) -> Header {
        let mut h = Header::new(self.frame_type, self.source.clone(), self.destination.clone());
        h.sequence = self.sequence;
        h.mcs = if self.n_data > 0 { self.mcs } else { 0 };
        h.n_control = n_control;
        h.n_data = self.n_data;
        h.session = self.is_session();
        h
    }

    /// The link state the frame is built for: broadcasts use the width with
    /// no mask and the probe to the width (ADR 0007); session frames the mask
    /// and this station's cap.
    pub fn link(&self) -> LinkState {
        if self.is_session() {
            LinkState { width: self.width, usable: self.usable.clone(), cp: Some(self.cp), cap: self.cap }
        } else {
            LinkState { width: self.width, usable: None, cp: Some(self.cp), cap: None }
        }
    }

    /// The frame's configuration (block counts and geometry).
    pub fn config(&self) -> Result<FrameConfig, String> {
        let controls = self.controls()?;
        let mut h = self.header(controls.len());
        h.probe = true;
        let probed = FrameConfig::from_header(&h, &self.link(), self.cp).map_err(|e| e.to_string())?;
        h.probe = probed.needs_probe() || matches!(self.frame_type, frame_type::CONNECT | frame_type::CONNECT_ACK);
        FrameConfig::from_header(&h, &self.link(), self.cp).map_err(|e| e.to_string())
    }
}

/// Build a frame with a real header. Payloads are `spec.message` padded with
/// zeros, or the simulator's random ones for `seed` (the same draws as
/// [`crate::frames::build_frame`], whose header bits are drawn and discarded),
/// so `cairn-bench rx` can score them. Returns the frame and its payloads.
pub fn build_header_frame(
    tx: &mut Transmitter,
    spec: &HeaderSpec,
    seed: u64,
) -> Result<(TxFrame, Vec<Vec<u8>>), String> {
    let controls = spec.controls()?;
    let mut header = spec.header(controls.len());
    if matches!(spec.frame_type, frame_type::CONNECT | frame_type::CONNECT_ACK) {
        header.probe = true; // SPEC 5.4: always
    }
    let payloads = payloads(spec, seed)?;
    let f = tx.build_frame(&header, &controls, &payloads, &spec.link()).map_err(|e| e.to_string())?;
    Ok((f, payloads))
}

fn payloads(spec: &HeaderSpec, seed: u64) -> Result<Vec<Vec<u8>>, String> {
    let n = spec.n_data;
    let bits = params::DATA_PAYLOAD_BITS;
    match &spec.message {
        Some(m) => {
            if m.len() > n * bits / 8 {
                return Err(format!("message of {} bytes does not fit {n} blocks of {} bytes", m.len(), bits / 8));
            }
            let mut bytes = m.clone();
            bytes.resize(n * bits / 8, 0);
            let all: Vec<u8> = bytes.iter().flat_map(|b| (0..8).rev().map(move |i| (b >> i) & 1)).collect();
            Ok(all.chunks(bits).map(<[u8]>::to_vec).collect())
        }
        None => {
            let mut rng = Pcg64::new(seed);
            let _ = rng.bits(params::HEADER_INFO_BITS - params::CRC_BITS);
            Ok(rng.bits(n * bits).chunks(bits).map(<[u8]>::to_vec).collect())
        }
    }
}

/// The payload bytes of the blocks (CRC excluded), MSB first.
pub fn payload_bytes(blocks: &[Vec<u8>]) -> Vec<u8> {
    blocks
        .iter()
        .flat_map(|b| b[..params::DATA_PAYLOAD_BITS].chunks(8).map(|c| c.iter().fold(0u8, |a, &v| (a << 1) | v)))
        .collect()
}

/// Parse `CALL` or `CALL-SSID` (SSID 0-15) into an address and the callsign text.
pub fn parse_station(text: &str) -> Result<(Address, String), String> {
    let t = text.trim().to_ascii_uppercase();
    if t == "*" || t == "ALL" || t == "BROADCAST" {
        return Ok((Address::Broadcast, String::new()));
    }
    let (call, ssid) = match t.rsplit_once('-') {
        Some((c, s)) if s.parse::<u8>().is_ok() => (c.to_string(), s.parse::<u8>().unwrap()),
        _ => (t.clone(), 0),
    };
    if ssid > 15 {
        return Err(format!("SSID {ssid} above 15 in {text:?}"));
    }
    let addr = Address::new(&call, ssid).map_err(|e| format!("{text:?}: {e}"))?;
    Ok((addr, call))
}

/// Usable tones of a width given the silenced ones (None when none are).
pub fn usable_from_silenced(width: usize, silenced: &[usize]) -> Option<Vec<usize>> {
    if silenced.is_empty() { None } else { Some((0..width).filter(|k| !silenced.contains(k)).collect()) }
}

/// SPEC 8.4 frame type name.
pub fn type_name(t: u8) -> String {
    match t {
        frame_type::BEACON => "BEACON".into(),
        frame_type::UI => "UI".into(),
        frame_type::CONNECT => "CONNECT".into(),
        frame_type::CONNECT_ACK => "CONNECT-ACK".into(),
        frame_type::DATA => "DATA".into(),
        frame_type::ACK => "ACK".into(),
        frame_type::SOUND => "SOUND".into(),
        frame_type::DISCONNECT => "DISCONNECT".into(),
        frame_type::PROBE => "PROBE".into(),
        frame_type::CAL_REQUEST => "CAL-REQUEST".into(),
        frame_type::POLL => "POLL".into(),
        frame_type::EXTENDED => "EXTENDED".into(),
        t => format!("type {t}"),
    }
}

/// Parse a frame type name (case-insensitive) or number.
pub fn parse_type(s: &str) -> Result<u8, String> {
    let u = s.to_ascii_uppercase().replace('_', "-");
    (0..32u8)
        .find(|&t| type_name(t) == u)
        .or_else(|| s.parse().ok().filter(|&t: &u8| t < 32))
        .ok_or_else(|| format!("unknown frame type {s:?} (UI, BEACON, DATA, PROBE, ...)"))
}

/// The header's flags in brackets (session, probe, more fragments, repeat), or nothing.
pub fn flag_text(h: &Header) -> String {
    let mut f = Vec::new();
    if h.session {
        f.push("session");
    }
    if h.probe {
        f.push("probe");
    }
    if h.more_fragments {
        f.push("more");
    }
    if h.repeat {
        f.push("repeat");
    }
    if f.is_empty() { String::new() } else { format!(" [{}]", f.join(",")) }
}

/// One line describing a control block (Appendix B).
pub fn control_summary(c: &ControlBlock) -> String {
    match c {
        ControlBlock::Ack(a) => {
            let mut s = format!(
                "ACK base {} bitmap {:#x} next MCS {} SNR {:.2} dB errors {}",
                a.base_sequence,
                a.bitmap,
                a.requested_mcs,
                a.snr_q as f64 / 4.0,
                a.block_errors
            );
            if let Some(m) = a.tone_mask {
                s += &format!(" mask {} tones", m.count_ones());
            }
            if a.reprobe {
                s += " re-probe";
            }
            if a.long_prefix {
                s += " long-prefix";
            }
            if a.distortion > 0 {
                s += &format!(" distortion {}", a.distortion);
            }
            if a.adc_clipped {
                s += " ADC-clipped";
            }
            s
        }
        ControlBlock::Connect(k) => format!(
            "CONNECT session {:#06x} caps {:#06x} cap tone {} next MCS {} max {:.1} s{}",
            k.session_id,
            k.capabilities,
            k.width_cap,
            k.requested_mcs,
            k.max_frame_time as f64 / 10.0,
            if k.resume { " resume" } else { "" }
        ),
        ControlBlock::CalRequest(r) => format!(
            "CAL-REQUEST {} Hz, {} steps of {} ms, gap {} ms, from -{:.2} dB by {} dB",
            r.tone_freq as u32 * 10,
            r.step_count,
            r.step_length as u32 * 10,
            r.gap_length as u32 * 10,
            r.start_level as f64 / 4.0,
            r.step_size
        ),
        ControlBlock::CalReport(r) => format!(
            "CAL-REPORT {} amplitudes -{:?} dBFS/4",
            if r.sweep_valid { "valid," } else { "invalid," },
            r.amplitudes
        ),
        ControlBlock::Id(i) => {
            let call = if i.ssid > 0 { format!("{}-{}", i.callsign, i.ssid) } else { i.callsign.clone() };
            match &i.grid {
                Some(g) => format!("ID {call} grid {}", g.trim()),
                None => format!("ID {call}"),
            }
        }
        ControlBlock::Path(p) => format!(
            "PATH hops {} via {}",
            p.hops,
            p.entries.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(",")
        ),
        ControlBlock::Unknown { control_type, .. } => format!("reserved type {control_type} (skipped)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_phy::{Receiver, RxOptions};

    #[test]
    fn stations_and_types() {
        let (a, t) = parse_station("n0call-7").unwrap();
        assert_eq!((a.to_string(), t.as_str()), ("N0CALL-7".to_string(), "N0CALL"));
        assert_eq!(parse_station("*").unwrap().0, Address::Broadcast);
        assert!(parse_station("N0CALL-16").is_err());
        assert_eq!(parse_type("ui").unwrap(), frame_type::UI);
        assert_eq!(parse_type("CAL-REQUEST").unwrap(), frame_type::CAL_REQUEST);
        assert_eq!(parse_type("4").unwrap(), frame_type::DATA);
        assert!(parse_type("nope").is_err());
    }

    #[test]
    fn message_payloads_round_trip() {
        let (me, call) = parse_station("N0CALL-1").unwrap();
        let mut s = HeaderSpec::broadcast(me, frame_type::UI, 5, 32, 1, 2);
        s.callsign = call;
        s.id = true;
        s.message = Some(b"CQ CQ de N0CALL test".to_vec());
        let (f, pl) = build_header_frame(&mut Transmitter::new(), &s, 1).unwrap();
        assert!(f.header.as_ref().unwrap().probe, "wide broadcast probes");
        assert_eq!(&payload_bytes(&pl)[..20], b"CQ CQ de N0CALL test");
        assert_eq!(s.config().unwrap(), f.config);
        let r = Receiver::new().receive(&f.samples, &f.config, 0, &RxOptions::default());
        assert!(r.block_ok.iter().all(|&b| b));
        assert_eq!(control_summary(r.controls[0].as_ref().unwrap()), "ID N0CALL-1");
        s.message = Some(vec![b'x'; 300]);
        assert!(build_header_frame(&mut Transmitter::new(), &s, 1).is_err());
    }
}
