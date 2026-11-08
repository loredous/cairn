//! KISS (SPEC 13.1): the framing, and a TCP server (default port 8001).
//!
//! Each data frame from a host is one AX.25 frame; it becomes one AX25 PDU
//! (SPEC 12.1 type 0) in a UI frame to the broadcast address (ADR 0010
//! `kiss-ui-frames`), several per UI frame when they fit, fragmented when
//! one does not. Every AX25 PDU received in a UI frame goes back to every
//! connected host as its own KISS data frame, unchanged. TXDELAY and slot
//! time (units of 10 ms) and persistence P (p = (P + 1)/256) map onto the
//! modem's parameters; TXtail, full duplex and SetHardware are accepted
//! and, apart from TXtail, ignored.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use cairn_link::Pdu;
use cairn_link::pdu::{encoding, pdu_type};

/// Frame end.
pub const FEND: u8 = 0xC0;
/// Frame escape.
pub const FESC: u8 = 0xDB;
/// Transposed frame end.
pub const TFEND: u8 = 0xDC;
/// Transposed frame escape.
pub const TFESC: u8 = 0xDD;

/// A KISS command from a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KissCommand {
    /// An AX.25 frame to send.
    Data(Vec<u8>),
    /// TXDELAY, units of 10 ms.
    TxDelay(u8),
    /// Persistence P: p = (P + 1)/256.
    Persistence(u8),
    /// Slot time, units of 10 ms.
    SlotTime(u8),
    /// TXtail, units of 10 ms.
    TxTail(u8),
    /// Full duplex (ignored: sessions are half duplex).
    FullDuplex(u8),
    /// SetHardware (ignored).
    SetHardware(Vec<u8>),
    /// Return (leave KISS mode): the server closes the connection.
    Return,
}

/// Escape and frame one KISS data frame on `port`.
pub fn encode(port: u8, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(data.len() + 4);
    v.push(FEND);
    v.push((port & 0x0F) << 4);
    for &b in data {
        match b {
            FEND => v.extend([FESC, TFEND]),
            FESC => v.extend([FESC, TFESC]),
            b => v.push(b),
        }
    }
    v.push(FEND);
    v
}

/// A streaming KISS decoder.
#[derive(Clone, Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    esc: bool,
    in_frame: bool,
}

/// Longest KISS frame accepted (an AX.25 frame is far shorter; a PDU body
/// may be up to 65,535 bytes less its fields).
const MAX_FRAME: usize = 65_000;

impl Decoder {
    /// Feed bytes; returns the commands completed, with their port.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<(u8, KissCommand)> {
        let mut out = Vec::new();
        for &b in bytes {
            match b {
                FEND => {
                    if self.in_frame && !self.buf.is_empty() {
                        if let Some(c) = parse(&self.buf) {
                            out.push(c);
                        }
                    }
                    self.buf.clear();
                    self.esc = false;
                    self.in_frame = true;
                }
                _ if !self.in_frame => {}
                FESC => self.esc = true,
                b => {
                    let v = if self.esc {
                        self.esc = false;
                        match b {
                            TFEND => FEND,
                            TFESC => FESC,
                            other => other,
                        }
                    } else {
                        b
                    };
                    if self.buf.len() < MAX_FRAME {
                        self.buf.push(v);
                    }
                }
            }
        }
        out
    }
}

fn parse(f: &[u8]) -> Option<(u8, KissCommand)> {
    let (&type_byte, rest) = f.split_first()?;
    if type_byte == 0xFF {
        return Some((0, KissCommand::Return));
    }
    let port = type_byte >> 4;
    let arg = rest.first().copied();
    let c = match type_byte & 0x0F {
        0 if !rest.is_empty() => KissCommand::Data(rest.to_vec()),
        1 => KissCommand::TxDelay(arg?),
        2 => KissCommand::Persistence(arg?),
        3 => KissCommand::SlotTime(arg?),
        4 => KissCommand::TxTail(arg?),
        5 => KissCommand::FullDuplex(arg?),
        6 => KissCommand::SetHardware(rest.to_vec()),
        _ => return None,
    };
    Some((port, c))
}

/// An AX.25 frame as an AX25 PDU (SPEC 12.1 type 0, raw, stream 0).
pub fn ax25_pdu(frame: Vec<u8>) -> Pdu {
    Pdu { pdu_type: pdu_type::AX25, stream: 0, encoding: encoding::RAW, dictionary_id: None, body: frame, crc: None }
}

/// The KISS server's side of the modem: write received AX.25 frames to
/// every connected host.
#[derive(Clone, Default)]
pub struct Hub {
    clients: Arc<Mutex<Vec<TcpStream>>>,
}

impl Hub {
    /// Send an AX.25 frame to every host (dropping hosts that went away).
    pub fn send(&self, frame: &[u8]) {
        let bytes = encode(0, frame);
        let mut c = self.clients.lock().expect("clients");
        c.retain_mut(|s| s.write_all(&bytes).is_ok());
    }

    /// Connected hosts.
    pub fn clients(&self) -> usize {
        self.clients.lock().expect("clients").len()
    }
}

/// Listen on `addr`; every command from every host goes to `tx` with the
/// host's address. Returns the hub, the bound address and the accept thread.
pub fn serve(addr: &str, tx: Sender<(SocketAddr, KissCommand)>) -> std::io::Result<(Hub, SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    let hub = Hub::default();
    let h2 = hub.clone();
    let accept = std::thread::Builder::new().name("kiss-accept".into()).spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let Ok(peer) = stream.peer_addr() else { continue };
            let _ = stream.set_nodelay(true);
            if let Ok(w) = stream.try_clone() {
                h2.clients.lock().expect("clients").push(w);
            }
            let tx = tx.clone();
            let _ = std::thread::Builder::new().name("kiss-client".into()).spawn(move || {
                let mut stream = stream;
                let mut dec = Decoder::default();
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            for (_, c) in dec.push(&buf[..n]) {
                                let ret = c == KissCommand::Return;
                                if tx.send((peer, c)).is_err() || ret {
                                    let _ = stream.shutdown(std::net::Shutdown::Both);
                                    return;
                                }
                            }
                        }
                    }
                }
            });
        }
    })?;
    Ok((hub, local, accept))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_round_trip() {
        let data = vec![0x82, FEND, 0x00, FESC, FESC, TFEND, 0x7E];
        let bytes = encode(0, &data);
        assert_eq!(bytes, vec![FEND, 0x00, 0x82, FESC, TFEND, 0x00, FESC, TFESC, FESC, TFESC, TFEND, 0x7E, FEND]);
        let mut d = Decoder::default();
        // Fed one byte at a time, with a leading FEND and garbage before it.
        let mut got = Vec::new();
        for b in [0x55].iter().chain(&[FEND]).chain(&bytes) {
            got.extend(d.push(&[*b]));
        }
        assert_eq!(got, vec![(0, KissCommand::Data(data))]);
    }

    #[test]
    fn parameters_and_ports() {
        let mut d = Decoder::default();
        let got = d
            .push(&[FEND, 0x01, 30, FEND, 0x12, 63, FEND, 0x03, 10, FEND, 0x14, 2, FEND, 0x06, 1, 2, FEND, 0xFF, FEND]);
        assert_eq!(
            got,
            vec![
                (0, KissCommand::TxDelay(30)),
                (1, KissCommand::Persistence(63)),
                (0, KissCommand::SlotTime(10)),
                (1, KissCommand::TxTail(2)),
                (0, KissCommand::SetHardware(vec![1, 2])),
                (0, KissCommand::Return),
            ]
        );
    }

    #[test]
    fn server_round_trip() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (hub, addr, _t) = serve("127.0.0.1:0", tx).unwrap();
        let mut c = TcpStream::connect(addr).unwrap();
        c.write_all(&encode(0, b"N0CALL>APRS:hello")).unwrap();
        let (_, cmd) = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(cmd, KissCommand::Data(b"N0CALL>APRS:hello".to_vec()));
        // Wait until the hub has the client, then send to it.
        for _ in 0..100 {
            if hub.clients() == 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        hub.send(b"frame\xC0back");
        c.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut d = Decoder::default();
        let mut buf = [0u8; 64];
        let mut got = Vec::new();
        while got.is_empty() {
            let n = c.read(&mut buf).unwrap();
            got.extend(d.push(&buf[..n]));
        }
        assert_eq!(got, vec![(0, KissCommand::Data(b"frame\xC0back".to_vec()))]);
    }
}
