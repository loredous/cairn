//! The native host API (SPEC 13.2, `docs/api.md`) end to end: complete
//! modems in the two-station simulator, driven only through API clients on
//! real sockets (TCP and a Unix socket), with every line the modems send
//! validated against `docs/api.schema.json`; unsupported commands and
//! malformed input; capture files against the audio that went in.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use cairn_api::proto::{MAX_LINE, Rejected, base64_decode};
use cairn_api::schema::Schema;
use cairn_api::server::{ApiServer, Inbound};
use cairn_api::{ApiError, Code};
use cairn_link::{Config, Instant};
use cairn_modem::hostapi::{HostApi, HostApiConfig};
use cairn_modem::runtime::{Modem, ModemConfig};
use cairn_modem::rxdir::RxDir;
use cairn_modem::twosim::Sim;
use serde_json::{Value, json};

/// Modem time zero is 2026-11-07T00:00:00Z in these tests.
const EPOCH_US: i64 = 1_794_009_600_000_000;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cairn-api-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn station(call: &str, seed: u64, ring_seconds: f64) -> ModemConfig {
    let mut c = Config::for_callsign(call, 0);
    c.seed = seed;
    let mut m = ModemConfig::new(c);
    m.rx.threads = 1;
    m.ring_seconds = ring_seconds;
    m
}

/// A modem's API side: its server, the requests arriving, the host state.
struct Node {
    srv: ApiServer,
    inbound: Receiver<Inbound>,
    api: HostApi,
    rx_dir: Option<RxDir>,
}

/// The simulator with an API server per station.
struct Harness {
    sim: Sim,
    nodes: Vec<Node>,
    schema: Schema,
    /// Lines the modems sent, all validated.
    checked: usize,
}

impl Harness {
    fn new(sim: Sim, capture_dir: &std::path::Path) -> Self {
        let mut nodes = Vec::new();
        let mut sim = sim;
        for i in 0..sim.stations.len() {
            sim.stations[i].outputs = Some(Vec::new());
            let (srv, inbound) = ApiServer::new();
            let cfg = HostApiConfig {
                capture_dir: capture_dir.join(format!("st{i}")),
                heard_file: None,
                epoch_unix_us: EPOCH_US,
            };
            let ring = sim.stations[i].modem.config().ring_seconds;
            nodes.push(Node { srv, inbound, api: HostApi::new(cfg, ring).unwrap(), rx_dir: None });
        }
        Harness { sim, nodes, schema: Schema::api(), checked: 0 }
    }

    fn check(&mut self, line: &str) {
        if let Err(e) = self.schema.validate_line(line) {
            panic!("schema: {e}");
        }
        self.checked += 1;
    }

    /// Requests in, outputs to events, events out; no simulated time passes.
    fn service(&mut self, i: usize) {
        while let Ok(m) = self.nodes[i].inbound.try_recv() {
            match m {
                Inbound::Connected(id) => {
                    let node = &mut self.nodes[i];
                    let h = self.sim.modem_call(i, |m, now| node.api.hello(id, m, now));
                    self.check(&h);
                    self.nodes[i].srv.send_hello(id, &h);
                }
                Inbound::Line(id, line) => {
                    let node = &mut self.nodes[i];
                    let h = self.sim.modem_call(i, |m, now| node.api.request(&line, m, now));
                    self.check(&h.response);
                    if let Some(s) = h.subscribe {
                        self.nodes[i].srv.subscribe(id, s);
                    }
                    self.nodes[i].srv.send(id, &h.response);
                }
                Inbound::TooLong(id) | Inbound::NotUtf8(id) => {
                    let error = match m {
                        Inbound::TooLong(_) => ApiError::new(Code::LineTooLong, "request line over 1 MiB"),
                        _ => ApiError::new(Code::BadJson, "request line is not UTF-8"),
                    };
                    let r = Rejected { id: Value::Null, cmd: Value::Null, error }.response().to_string();
                    self.check(&r);
                    self.nodes[i].srv.send(id, &r);
                }
                Inbound::Disconnected(_) => {}
            }
        }
        let outs = std::mem::take(self.sim.stations[i].outputs.as_mut().expect("outputs"));
        for (t, o) in outs {
            let node = &mut self.nodes[i];
            let modem = &self.sim.stations[i].modem;
            match o {
                Some(o) => {
                    let saved = match (&o, node.rx_dir.as_mut()) {
                        (cairn_modem::runtime::Output::Deliver { from, pdu, .. }, Some(r)) => {
                            r.save(*from, pdu, false).unwrap()
                        }
                        _ => None,
                    };
                    node.api.on_output(&o, saved.as_ref(), modem, t);
                }
                None => node.api.on_tx_done(None, t),
            }
        }
        let now = self.sim.now();
        self.nodes[i].api.poll(&self.sim.stations[i].modem, now);
        for e in self.nodes[i].api.take_events() {
            self.check(&e.line);
            self.nodes[i].srv.publish(e.name, &e.line);
        }
    }

    fn service_all(&mut self) {
        for i in 0..self.nodes.len() {
            self.service(i);
        }
    }

    /// Simulate `dt` in 10 ms steps, serving the APIs after each.
    fn step(&mut self, dt: Duration) {
        let end = self.sim.now() + dt;
        while self.sim.now() < end {
            let next = self.sim.now() + Duration::from_millis(10);
            self.sim.run_until(next);
            self.service_all();
        }
    }
}

/// An API client on a socket: lines in on a reader thread.
struct Client {
    w: Box<dyn Write + Send>,
    rx: Receiver<String>,
    /// Every line received.
    lines: Vec<Value>,
    next_id: i64,
}

impl Client {
    fn tcp(addr: std::net::SocketAddr) -> Client {
        let s = std::net::TcpStream::connect(addr).unwrap();
        let r = s.try_clone().unwrap();
        Client::start(Box::new(s), Box::new(r))
    }

    #[cfg(unix)]
    fn unix(path: &std::path::Path) -> Client {
        let s = std::os::unix::net::UnixStream::connect(path).unwrap();
        let r = s.try_clone().unwrap();
        Client::start(Box::new(s), Box::new(r))
    }

    fn start(w: Box<dyn Write + Send>, r: Box<dyn std::io::Read + Send>) -> Client {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for l in BufReader::new(r).lines() {
                let Ok(l) = l else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        Client { w, rx, lines: Vec::new(), next_id: 1 }
    }

    fn drain(&mut self) {
        while let Ok(l) = self.rx.try_recv() {
            self.lines.push(serde_json::from_str(&l).unwrap_or_else(|e| panic!("client got non-JSON {l}: {e}")));
        }
    }

    fn raw(&mut self, bytes: &[u8]) {
        self.w.write_all(bytes).unwrap();
        self.w.flush().unwrap();
    }

    fn events(&self, name: &str) -> Vec<&Value> {
        self.lines.iter().filter(|v| v["event"] == name).collect()
    }

    fn has(&self, f: impl Fn(&Value) -> bool) -> bool {
        self.lines.iter().any(f)
    }
}

/// Send a request and serve until its response arrives (no simulated time
/// passes).
fn request(h: &mut Harness, c: &mut Client, mut req: Value) -> Value {
    let id = c.next_id;
    c.next_id += 1;
    req["id"] = json!(id);
    c.raw(format!("{req}\n").as_bytes());
    wait_response(h, c, json!(id))
}

fn wait_response(h: &mut Harness, c: &mut Client, id: Value) -> Value {
    let t0 = std::time::Instant::now();
    loop {
        h.service_all();
        c.drain();
        if let Some(r) = c.lines.iter().find(|v| v.get("ok").is_some() && v["id"] == id) {
            return r.clone();
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "no response to {id}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Wait (wall time) for the hello line.
fn hello(h: &mut Harness, c: &mut Client) -> Value {
    let t0 = std::time::Instant::now();
    loop {
        h.service_all();
        c.drain();
        if let Some(v) = c.lines.first() {
            return v.clone();
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "no hello");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Simulate until `cond` holds for the clients, at most `limit` seconds.
fn run_until(h: &mut Harness, clients: &mut [&mut Client], limit: f64, cond: impl Fn(&[&mut Client]) -> bool) {
    let end = h.sim.now() + Duration::from_secs_f64(limit);
    loop {
        h.step(Duration::from_millis(100));
        // Lines may still be in flight on the sockets.
        std::thread::sleep(Duration::from_micros(200));
        for c in clients.iter_mut() {
            c.drain();
        }
        if cond(clients) {
            return;
        }
        assert!(h.sim.now() < end, "condition not met within {limit} s of simulated time");
    }
}

fn payload(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

#[test]
fn two_stations_driven_through_the_api() {
    let dir = tmp("two");
    let mut sim = Sim::new();
    sim.add(station("N0AAA", 41, 30.0), 20.0, 25.0, 41);
    sim.add(station("N0BBB", 42, 30.0), -20.0, 25.0, 42);
    let mut h = Harness::new(sim, &dir);
    h.nodes[1].rx_dir = Some(RxDir::new(&dir.join("rx")).unwrap());
    let addr_a = h.nodes[0].srv.listen_tcp("127.0.0.1:0").unwrap();
    let sock_b = dir.join("b.sock");
    h.nodes[1].srv.listen_unix(&sock_b).unwrap();

    let mut a = Client::tcp(addr_a);
    let mut b = Client::unix(&sock_b);
    let mut a_heard = Client::tcp(addr_a);
    let ha = hello(&mut h, &mut a);
    assert_eq!(ha["event"], "hello");
    assert_eq!(ha["api_version"], "0.2.0");
    assert_eq!(ha["station"], "N0AAA");
    assert_eq!(ha["unsupported"], json!(["calibrate"]));
    assert_eq!(hello(&mut h, &mut b)["station"], "N0BBB");
    hello(&mut h, &mut a_heard);
    let r = request(&mut h, &mut a_heard, json!({"cmd": "subscribe", "events": ["heard"]}));
    assert_eq!(r["events"], json!(["heard"]));

    let st = request(&mut h, &mut a, json!({"cmd": "status"}));
    assert_eq!((st["ok"].clone(), st["session"].clone()), (json!(true), Value::Null));
    let cfg = request(&mut h, &mut a, json!({"cmd": "config"}));
    assert_eq!(cfg["config"]["cap"], 24);

    // Connect.
    let r = request(&mut h, &mut a, json!({"cmd": "connect", "peer": "n0bbb"}));
    assert_eq!(
        (r["ok"].clone(), r["peer"].clone(), r["resuming"].clone()),
        (json!(true), json!("N0BBB"), json!(false))
    );
    let open = |c: &Client| c.has(|v| v["event"] == "session" && v["state"] == "open");
    run_until(&mut h, &mut [&mut a, &mut b], 30.0, |cs| open(cs[0]) && open(cs[1]));
    assert!(a.has(|v| v["event"] == "session" && v["state"] == "connecting"));
    let busy = request(&mut h, &mut a, json!({"cmd": "connect", "peer": "N0BBB"}));
    assert_eq!(busy["error"]["code"], "busy");

    // A file and a message.
    let data = payload(9_000, 7);
    let file = dir.join("report.bin");
    std::fs::write(&file, &data).unwrap();
    let rf = request(&mut h, &mut a, json!({"cmd": "send", "file": file.display().to_string()}));
    assert_eq!((rf["ok"].clone(), rf["bytes"].clone(), rf["pdus"].clone()), (json!(true), json!(9000), json!(5)));
    let rt = request(&mut h, &mut a, json!({"cmd": "send", "text": "hello from N0AAA", "stream": 2}));
    assert_eq!(rt["encoding"], "raw");
    let (ref_f, ref_t) = (rf["ref"].clone(), rt["ref"].clone());
    let delivered =
        move |c: &Client, r: &Value| c.has(|v| v["event"] == "session" && v["kind"] == "delivered" && v["ref"] == *r);
    let (rf2, rt2) = (ref_f.clone(), ref_t.clone());
    run_until(&mut h, &mut [&mut a, &mut b], 200.0, move |cs| delivered(cs[0], &rf2) && delivered(cs[0], &rt2));

    // The receiver's rx events rebuild the file and carry the message.
    let mut rebuilt = vec![0u8; data.len()];
    let mut got = 0;
    for v in b.events("rx") {
        assert_eq!(
            (v["from"].clone(), v["mode"].clone(), v["verify"].clone()),
            (json!("N0AAA"), json!("session"), json!("unsigned"))
        );
        let bytes = base64_decode(v["data"].as_str().unwrap()).unwrap();
        assert_eq!(bytes.len() as u64, v["bytes"].as_u64().unwrap());
        if v["pdu_type"] == "file_chunk" {
            assert_eq!(v["priority"], 3, "a file goes at priority 3 by default");
            let off = v["offset"].as_u64().unwrap() as usize;
            rebuilt[off..off + bytes.len()].copy_from_slice(&bytes);
            got += bytes.len();
        }
    }
    assert_eq!(got, data.len());
    assert_eq!(rebuilt, data);
    let msg = b.events("rx").into_iter().find(|v| v["pdu_type"] == "message").expect("message").clone();
    assert_eq!(
        (msg["text"].clone(), msg["stream"].clone(), msg["priority"].clone()),
        (json!("hello from N0AAA"), json!(2), json!(0))
    );
    let done = b.events("rx").into_iter().find(|v| v["saved"]["complete"] == true && v["pdu_type"] == "file_chunk");
    assert_eq!(done.expect("a saved, complete file")["saved"]["sha256"], cairn_modem::rxdir::sha256_hex(&data));

    // Progress, link quality, heard.
    let prog = a.events("session").into_iter().rfind(|v| v["kind"] == "progress").expect("progress").clone();
    assert_eq!(prog["progress"]["tx"]["acked_bytes"], 9000 + 16);
    assert_eq!(prog["progress"]["tx"]["pending_bytes"], 0);
    assert!(prog["progress"]["blocks_sent"].as_u64().unwrap() > 0);
    let b_prog = b.events("session").into_iter().rfind(|v| v["kind"] == "progress").expect("progress").clone();
    assert_eq!(b_prog["progress"]["rx"]["delivered_bytes"], 9000 + 16);
    let links = a.events("link");
    assert!(!links.is_empty());
    for l in &links {
        assert_eq!(l["peer"], "N0BBB");
        assert!(l["snr_db"].as_f64().unwrap() > 18.0, "{l}");
    }
    assert!(links.iter().any(|l| !l["far_end"].is_null()), "the peer's ACK reports reach the link events");
    assert!(b.events("link").iter().any(|l| l["blocks"].as_u64().unwrap() > 0));
    assert!(a.events("heard").iter().any(|v| v["address"] == "N0BBB" && v["frame_type"] == "CONNECT-ACK"));
    assert!(b.events("heard").iter().any(|v| v["address"] == "N0AAA" && v["frame_type"] == "DATA"));
    assert!(a.has(|v| v["event"] == "tx" && v["kind"] == "start" && v["frame_type"] == "CONNECT"));
    assert!(a.has(|v| v["event"] == "tx" && v["kind"] == "done"));
    assert!(a.has(|v| v["event"] == "channel" && v["holder"] == "self"));
    assert!(a.has(|v| v["event"] == "channel" && v["holder"] == "peer"), "the peer's frames hold the channel");
    assert!(a.has(|v| v["event"] == "channel" && v["turn"] == "peer"));
    assert!(a.has(|v| v["event"] == "channel" && !v["waiting"].is_null()));
    assert!(a.has(|v| v["event"] == "dcd" && v["raw"] == true));
    let heard = request(&mut h, &mut a, json!({"cmd": "heard"}));
    let e = &heard["stations"][0];
    assert_eq!(e["address"], "N0BBB");
    assert!(e["count"].as_u64().unwrap() >= 3, "{e}");
    assert!(e["first_heard"].as_str().unwrap().starts_with("2026-11-07T00:0"), "{e}");
    let st = request(&mut h, &mut a, json!({"cmd": "status"}));
    assert_eq!(st["session"]["phase"], "open");
    assert_eq!(st["session"]["progress"]["tx"]["acked_bytes"], 9016);
    assert_eq!(st["heard_stations"], 1);

    // A capture at the receiver holds the frames it decoded.
    let cap = request(&mut h, &mut b, json!({"cmd": "capture", "seconds": 10, "label": "xfer"}));
    assert_eq!(cap["ok"], true, "{cap}");
    let side: Value =
        serde_json::from_str(&std::fs::read_to_string(cap["sidecar"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(side["format"], "cairn-capture/1");
    assert_eq!(side["samples"], 10 * 48_000);
    assert!(side["frames"].as_array().unwrap().iter().any(|f| f["src"] == "N0AAA"), "{}", side["frames"]);
    assert!(side["dcd"].as_array().unwrap().len() > 1);
    assert_eq!(side["link"]["session"]["peer"], "N0AAA");
    let wav = cairn_phy::wav::read(std::path::Path::new(cap["wav"].as_str().unwrap())).unwrap();
    assert_eq!(wav.len(), 10 * 48_000);

    // Disconnect.
    let r = request(&mut h, &mut a, json!({"cmd": "disconnect"}));
    assert_eq!((r["ok"].clone(), r["graceful"].clone()), (json!(true), json!(true)));
    let closed = |c: &Client| c.has(|v| v["event"] == "session" && v["state"] == "disconnected");
    run_until(&mut h, &mut [&mut a, &mut b], 60.0, |cs| closed(cs[0]) && closed(cs[1]));
    assert!(a.has(|v| v["event"] == "session" && v["state"] == "closing"));
    let r = request(&mut h, &mut a, json!({"cmd": "disconnect"}));
    assert_eq!(r["error"]["code"], "no_session");

    // The heard-only client got nothing but heard events once its
    // subscription was answered.
    a_heard.drain();
    let from = a_heard.lines.iter().position(|v| v["cmd"] == "subscribe").expect("subscribe response");
    let names: Vec<&Value> = a_heard.lines[from..].iter().filter_map(|v| v.get("event")).collect();
    assert!(!names.is_empty());
    assert!(names.iter().all(|n| *n == "heard"), "{names:?}");
    if let Some(p) = std::env::var_os("API_TEST_DUMP") {
        // Every line each client received, for reading.
        let text: Vec<String> = a.lines.iter().chain(&b.lines).map(|v| v.to_string()).collect();
        std::fs::write(p, text.join("\n") + "\n").unwrap();
    }
    eprintln!("{} lines validated against docs/api.schema.json", h.checked);
    assert!(h.checked > 100);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The repository's dictionary directory (the evaluation dictionaries).
#[cfg(feature = "zstd")]
fn eval_dictionaries() -> cairn_link::compress::Dictionaries {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dictionaries/eval");
    cairn_link::compress::Dictionaries::load_dir(&dir).unwrap().0
}

/// SPEC 12.2 through the API: dictionaries negotiated at setup, `auto`
/// sends compressed against them, `rx` events carry decoded bodies with
/// their encoding, and a broadcast naming a dictionary the receiver lacks
/// arrives undecoded with a warning.
#[test]
#[cfg(feature = "zstd")]
fn compressed_sends_through_the_api() {
    let dir = tmp("zstd");
    let mut sim = Sim::new();
    let mut a = station("N0AAA", 61, 0.0);
    a.dictionaries = eval_dictionaries();
    let mut b = station("N0BBB", 62, 0.0);
    let mut only_64k = cairn_link::compress::Dictionaries::new();
    only_64k.insert(a.dictionaries.iter().find(|d| d.bytes().len() == 65_536).unwrap().clone());
    b.dictionaries = only_64k;
    sim.add(a, 20.0, 25.0, 61);
    sim.add(b, -20.0, 25.0, 62);
    let mut h = Harness::new(sim, &dir);
    let addr_a = h.nodes[0].srv.listen_tcp("127.0.0.1:0").unwrap();
    let addr_b = h.nodes[1].srv.listen_tcp("127.0.0.1:0").unwrap();
    let mut a = Client::tcp(addr_a);
    let mut b = Client::tcp(addr_b);
    assert_eq!(hello(&mut h, &mut a)["encodings"], json!(["raw", "zstd", "zstd_dict"]));
    hello(&mut h, &mut b);
    let st = request(&mut h, &mut a, json!({"cmd": "status"}));
    assert_eq!(st["dictionaries"].as_array().unwrap().len(), 3, "{st}");

    request(&mut h, &mut a, json!({"cmd": "connect", "peer": "N0BBB"}));
    let open = |c: &Client| c.has(|v| v["event"] == "session" && v["state"] == "open");
    run_until(&mut h, &mut [&mut a, &mut b], 30.0, |cs| open(cs[0]) && open(cs[1]));
    let st = request(&mut h, &mut a, json!({"cmd": "status"}));
    assert_eq!(st["session"]["dictionaries"], json!(["80301843"]), "the one dictionary both have");

    let text = "ICS 213 GENERAL MESSAGE. TO: Planning Section Chief. FROM: Communications Unit Leader. \
                SUBJECT: Repeater status. MESSAGE: The county repeater on 146.940 is back on emergency power; \
                net control will resume at 1900 local. Please relay to all field teams. APPROVED: J. Smith, COML.";
    let r_auto = request(&mut h, &mut a, json!({"cmd": "send", "text": text}));
    assert_eq!((r_auto["encoding"].clone(), r_auto["dictionary"].clone()), (json!("zstd_dict"), json!("80301843")));
    assert_eq!(r_auto["pdu_bytes"], 2 + 3 + 4 + 129 + 4, "the vector's 129-byte body: {r_auto}");
    let r_plain = request(&mut h, &mut a, json!({"cmd": "send", "text": text, "encoding": "zstd"}));
    assert_eq!((r_plain["encoding"].clone(), r_plain["pdu_bytes"].clone()), (json!("zstd"), json!(2 + 3 + 219 + 4)));
    let r_raw = request(&mut h, &mut a, json!({"cmd": "send", "text": text, "encoding": "raw"}));
    assert_eq!(r_raw["pdu_bytes"], 2 + 3 + text.len() + 4);
    let r_bad = request(
        &mut h,
        &mut a,
        json!({"cmd": "send", "text": text, "encoding": "zstd_dict", "dictionary": "45f5d308"}),
    );
    assert_eq!(r_bad["error"]["code"], "unknown_dictionary", "A has it, but B did not list it: {r_bad}");
    // A text file: FILE-CHUNKs compressed one by one.
    let doc = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dictionaries/corpus/ecfr_part97/part97.txt"),
    )
    .unwrap();
    let data = &doc[..9_000];
    let file = dir.join("part97.txt");
    std::fs::write(&file, data).unwrap();
    let r_file = request(&mut h, &mut a, json!({"cmd": "send", "file": file.display().to_string()}));
    assert_ne!(r_file["encoding"], "raw", "{r_file}");
    assert!(r_file["pdu_bytes"].as_u64().unwrap() < 9_000 * 3 / 4, "{r_file}");
    let refs: Vec<Value> = [&r_auto, &r_plain, &r_raw, &r_file].iter().map(|r| r["ref"].clone()).collect();
    let delivered =
        |c: &Client, r: &Value| c.has(|v| v["event"] == "session" && v["kind"] == "delivered" && v["ref"] == *r);
    let refs2 = refs.clone();
    run_until(&mut h, &mut [&mut a, &mut b], 200.0, move |cs| refs2.iter().all(|r| delivered(cs[0], r)));

    let rx = b.events("rx");
    let msgs: Vec<&&Value> = rx.iter().filter(|v| v["pdu_type"] == "message").collect();
    assert_eq!(msgs.len(), 3);
    let encs: Vec<&str> = msgs.iter().map(|v| v["encoding"].as_str().unwrap()).collect();
    assert_eq!(encs, ["zstd_dict", "zstd", "raw"]);
    for m in &msgs {
        assert_eq!(
            (m["text"].as_str(), m["decoded"].clone(), m["crc"].clone()),
            (Some(text), json!(true), json!("ok"))
        );
    }
    assert_eq!(msgs[0]["dictionary"], "80301843");
    assert_eq!((msgs[0]["encoded_bytes"].clone(), msgs[1]["encoded_bytes"].clone()), (json!(129), json!(219)));
    let mut rebuilt = vec![0u8; data.len()];
    for v in rx.iter().filter(|v| v["pdu_type"] == "file_chunk") {
        let bytes = base64_decode(v["data"].as_str().unwrap()).unwrap();
        let off = v["offset"].as_u64().unwrap() as usize;
        rebuilt[off..off + bytes.len()].copy_from_slice(&bytes);
    }
    assert_eq!(rebuilt, data);

    // B to A's 32 kB dictionary in a broadcast: B lacks it, so it is
    // refused at send; A to B with that dictionary arrives undecoded.
    let r = request(
        &mut h,
        &mut a,
        json!({"cmd": "send", "mode": "broadcast", "text": text, "encoding": "zstd_dict", "dictionary": "45f5d308"}),
    );
    assert_eq!(r["encoding"], "zstd_dict", "{r}");
    let undecoded = |c: &Client| c.has(|v| v["event"] == "rx" && v["mode"] == "broadcast");
    run_until(&mut h, &mut [&mut a, &mut b], 60.0, |cs| undecoded(cs[1]));
    let v = b.events("rx").into_iter().find(|v| v["mode"] == "broadcast").unwrap().clone();
    assert_eq!(
        (v["decoded"].clone(), v["crc"].clone(), v["dictionary"].clone()),
        (json!(false), json!("unchecked"), json!("45f5d308"))
    );
    assert!(v.get("text").is_none());
    assert!(b.has(|e| e["event"] == "error" && e["code"] == "unknown_dictionary" && e["severity"] == "warning"));
    eprintln!("{} lines validated against docs/api.schema.json", h.checked);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unsupported_and_malformed_requests() {
    let dir = tmp("errors");
    let mut sim = Sim::new();
    sim.add(station("N0AAA", 51, 4.0), 0.0, 25.0, 51);
    let mut h = Harness::new(sim, &dir);
    let addr = h.nodes[0].srv.listen_tcp("127.0.0.1:0").unwrap();
    let mut c = Client::tcp(addr);
    hello(&mut h, &mut c);
    let code = |v: &Value| v["error"]["code"].as_str().unwrap_or("").to_string();

    let r = request(&mut h, &mut c, json!({"cmd": "calibrate", "peer": "N0BBB"}));
    assert_eq!((r["ok"].clone(), code(&r)), (json!(false), "unsupported".into()));
    assert_eq!(r["cmd"], "calibrate");
    let zstd = cfg!(feature = "zstd");
    let r = request(&mut h, &mut c, json!({"cmd": "send", "text": "x", "encoding": "zstd"}));
    assert_eq!(code(&r), if zstd { "no_session" } else { "unsupported" });
    let r = request(&mut h, &mut c, json!({"cmd": "send", "text": "x", "encoding": "lz4"}));
    assert_eq!(code(&r), "bad_request");
    let bcast = |enc: &str, dict: Option<&str>| {
        let mut v = json!({"cmd": "send", "mode": "broadcast", "text": "x", "encoding": enc});
        if let Some(d) = dict {
            v["dictionary"] = json!(d);
        }
        v
    };
    let r = request(&mut h, &mut c, bcast("zstd_dict", Some("0a0b0c0d")));
    assert_eq!(code(&r), if zstd { "unknown_dictionary" } else { "unsupported" }, "{r}");
    let r = request(&mut h, &mut c, bcast("zstd_dict", None));
    assert_eq!(code(&r), if zstd { "unknown_dictionary" } else { "unsupported" }, "no dictionary named: {r}");
    let r = request(&mut h, &mut c, bcast("auto", Some("0a0b0c0d")));
    assert_eq!(code(&r), "bad_request", "a dictionary goes with zstd_dict");
    let r = request(&mut h, &mut c, bcast("zstd_dict", Some("0A0B0C0D")));
    assert_eq!(code(&r), "bad_request");
    // A 4-byte broadcast is sent raw by auto; forced zstd is larger but obeyed.
    let r = request(&mut h, &mut c, bcast("auto", None));
    assert_eq!((r["encoding"].clone(), r["pdu_bytes"].clone()), (json!("raw"), json!(10)), "{r}");
    let r = request(&mut h, &mut c, bcast("zstd", None));
    if zstd {
        assert_eq!(r["encoding"], "zstd", "{r}");
        assert!(r["pdu_bytes"].as_u64().unwrap() > 10, "{r}");
    } else {
        assert_eq!(code(&r), "unsupported");
    }
    let r = request(&mut h, &mut c, json!({"cmd": "send", "text": "x"}));
    assert_eq!(code(&r), "no_session");
    let r = request(&mut h, &mut c, json!({"cmd": "send"}));
    assert_eq!(code(&r), "bad_request");
    let r = request(&mut h, &mut c, json!({"cmd": "send", "text": "x", "data": "eA=="}));
    assert_eq!(code(&r), "bad_request");
    let r = request(&mut h, &mut c, json!({"cmd": "send", "mode": "broadcast", "data": "not base64!"}));
    assert_eq!(code(&r), "bad_request");
    let r = request(&mut h, &mut c, json!({"cmd": "send", "mode": "broadcast", "text": "x", "priority": 9}));
    assert_eq!(code(&r), "bad_request");
    let r = request(&mut h, &mut c, json!({"cmd": "send", "mode": "broadcast", "file": "/nonexistent/cairn"}));
    assert_eq!(code(&r), "not_found");
    let big = "y".repeat(70_000);
    let r = request(&mut h, &mut c, json!({"cmd": "send", "mode": "broadcast", "text": big}));
    assert_eq!(code(&r), "too_large");
    let r = request(&mut h, &mut c, json!({"cmd": "connect"}));
    assert_eq!(code(&r), "bad_request");
    let r = request(&mut h, &mut c, json!({"cmd": "connect", "peer": "not a call"}));
    assert_eq!(code(&r), "bad_request");
    let r = request(&mut h, &mut c, json!({"cmd": "probe", "target": "N0BBB", "kind": "sideways"}));
    assert_eq!(code(&r), "bad_request");
    let r = request(&mut h, &mut c, json!({"cmd": "heard", "since": "yesterday"}));
    assert_eq!(code(&r), "bad_request");
    let r = request(&mut h, &mut c, json!({"cmd": "subscribe", "events": ["bogus"]}));
    assert_eq!(code(&r), "bad_request");
    let r = request(&mut h, &mut c, json!({"cmd": "capture", "label": "../etc"}));
    assert_eq!(code(&r), "bad_request");
    let r = request(&mut h, &mut c, json!({"cmd": "frobnicate"}));
    assert_eq!((code(&r), r["cmd"].clone()), ("unknown_command".into(), json!("frobnicate")));

    // Config: unknown, read-only, out of range, atomic, then a change.
    let r = request(&mut h, &mut c, json!({"cmd": "config", "set": {"warp": 9}}));
    assert_eq!(code(&r), "bad_request");
    let r = request(&mut h, &mut c, json!({"cmd": "config", "set": {"callsign": "N0ZZZ"}}));
    assert_eq!(code(&r), "read_only");
    let r = request(&mut h, &mut c, json!({"cmd": "config", "set": {"persistence": 2.0}}));
    assert_eq!(code(&r), "out_of_range");
    let r = request(&mut h, &mut c, json!({"cmd": "config", "set": {"persistence": 0.5, "slot_ms": 5}}));
    assert_eq!(code(&r), "out_of_range");
    let r = request(&mut h, &mut c, json!({"cmd": "config"}));
    assert_eq!(r["config"]["persistence"], 0.25, "a failed set changes nothing");
    let r = request(
        &mut h,
        &mut c,
        json!({"cmd": "config", "set": {"persistence": 0.5, "txdelay_ms": 200, "cap": 32, "broadcast_mcs": 1}}),
    );
    assert_eq!(r["changed"], json!(["persistence", "txdelay_ms", "cap", "broadcast_mcs"]), "{r}");
    assert_eq!(r["config"]["width_cap"], 31);
    assert_eq!(r["config"]["quieting_hz"], json!([3600.0, 4150.0]));
    assert_eq!(h.sim.stations[0].modem.config().txdelay, Duration::from_millis(200));
    assert_eq!(h.sim.stations[0].modem.station().config().persistence, 0.5);

    // Malformed lines: each answered, the connection still works.
    c.raw(b"{nope\n");
    let r = wait_response(&mut h, &mut c, Value::Null);
    assert_eq!((code(&r), r["cmd"].clone()), ("bad_json".into(), Value::Null));
    c.lines.clear();
    c.raw(b"[1,2,3]\n");
    assert_eq!(code(&wait_response(&mut h, &mut c, Value::Null)), "bad_json");
    c.lines.clear();
    c.raw(b"{\"id\":77}\n");
    assert_eq!(code(&wait_response(&mut h, &mut c, json!(77))), "bad_request");
    c.raw(b"{\"id\":{\"x\":1},\"cmd\":\"status\"}\n");
    assert_eq!(code(&wait_response(&mut h, &mut c, Value::Null)), "bad_request");
    c.lines.clear();
    c.raw(b"\xc3\x28\n");
    assert_eq!(code(&wait_response(&mut h, &mut c, Value::Null)), "bad_json");
    c.lines.clear();
    let mut long = vec![b'{'; MAX_LINE + 1];
    long.push(b'\n');
    c.raw(&long);
    assert_eq!(code(&wait_response(&mut h, &mut c, Value::Null)), "line_too_long");
    c.raw(b"\n  \r\n");
    let r = request(&mut h, &mut c, json!({"cmd": "status"}));
    assert_eq!(r["ok"], true);
    // A string id is echoed.
    c.raw(b"{\"id\":\"q-1\",\"cmd\":\"heard\"}\n");
    let r = wait_response(&mut h, &mut c, json!("q-1"));
    assert_eq!(r["stations"], json!([]));

    // Broadcasts and probes are accepted without a session.
    let r = request(&mut h, &mut c, json!({"cmd": "send", "mode": "broadcast", "text": "qst", "repeats": 1}));
    assert_eq!(r["mode"], "broadcast");
    let r = request(&mut h, &mut c, json!({"cmd": "beacon", "text": "N0AAA test"}));
    assert_eq!(r["ok"], true);
    run_until(&mut h, &mut [&mut c], 30.0, |cs| {
        cs[0].lines.iter().filter(|v| v["event"] == "tx" && v["kind"] == "broadcast_sent").count() >= 2
    });
    let r = request(&mut h, &mut c, json!({"cmd": "probe", "target": "N0BBB", "repeats": 0}));
    let pref = r["ref"].clone();
    run_until(&mut h, &mut [&mut c], 30.0, move |cs| {
        cs[0].has(|v| v["event"] == "link" && v["kind"] == "probe" && v["ref"] == pref && v["answered"] == false)
    });
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn capture_is_unsupported_without_a_ring() {
    let dir = tmp("noring");
    let mut sim = Sim::new();
    sim.add(station("N0AAA", 61, 0.0), 0.0, 25.0, 61);
    let mut h = Harness::new(sim, &dir);
    let addr = h.nodes[0].srv.listen_tcp("127.0.0.1:0").unwrap();
    let mut c = Client::tcp(addr);
    let hi = hello(&mut h, &mut c);
    assert_eq!(hi["unsupported"], json!(["calibrate", "capture"]));
    let r = request(&mut h, &mut c, json!({"cmd": "capture"}));
    assert_eq!(r["error"]["code"], "unsupported");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ptt_fault_is_a_schema_valid_warning_event() {
    let dir = tmp("pttfault");
    let mut api =
        HostApi::new(HostApiConfig { capture_dir: dir.clone(), heard_file: None, epoch_unix_us: EPOCH_US }, 2.0)
            .unwrap();
    api.on_ptt_fault("PTT release failed: Broken pipe. The transmitter MAY STILL BE KEYED", Instant(1_000));
    let evs = api.take_events();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].name, "error");
    let v: serde_json::Value = serde_json::from_str(&evs[0].line).unwrap();
    assert_eq!((v["code"].as_str(), v["severity"].as_str()), (Some("ptt_fault"), Some("warning")));
    Schema::api().validate_line(&evs[0].line).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn capture_wav_holds_exactly_the_audio_that_went_in() {
    let dir = tmp("capture");
    let mut modem = Modem::new(station("N0AAA", 71, 2.0));
    let mut api =
        HostApi::new(HostApiConfig { capture_dir: dir.clone(), heard_file: None, epoch_unix_us: EPOCH_US }, 2.0)
            .unwrap();
    // 3 s of a tone with a slow ramp and a little pseudo-noise, in 10 ms chunks.
    let n = 3 * 48_000;
    let mut x = 0x1234_5678u64;
    let audio: Vec<f64> = (0..n)
        .map(|i| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let t = i as f64 / 48_000.0;
            0.3 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()
                + 0.1 * t / 3.0
                + ((x >> 40) as f64 / 2f64.powi(24) - 0.5) * 0.01
        })
        .collect();
    for (k, chunk) in audio.chunks(480).enumerate() {
        let t = Instant((k as u64 * 480 * 1_000_000) / 48_000);
        modem.push_audio(chunk, t, t);
    }
    let now = modem.time_of(modem.position());
    for o in modem.drain() {
        api.on_output(&o, None, &modem, now);
    }
    let q = |v: f64| (v * 32768.0).round().clamp(-32768.0, 32767.0) / 32768.0;
    let schema = Schema::api();
    let check = |m: &serde_json::Map<String, Value>| {
        let mut v = Value::Object(m.clone());
        v["id"] = json!(1);
        v["ok"] = json!(true);
        v["cmd"] = json!("capture");
        schema.validate(&v).unwrap();
    };

    // The last 1.5 s.
    let m = api.capture(&modem, Some(1.5), Some("tone"), "api").unwrap();
    check(&m);
    assert_eq!(m["samples"], 72_000);
    assert!(m["wav"].as_str().unwrap().ends_with("-tone.wav"));
    let wav = cairn_phy::wav::read(std::path::Path::new(m["wav"].as_str().unwrap())).unwrap();
    let want: Vec<f64> = audio[n - 72_000..].iter().map(|&v| q(v)).collect();
    assert_eq!(wav, want);
    assert!((m["start_modem_time"].as_f64().unwrap() - 1.5).abs() < 1e-3, "{m:?}");
    let side: Value = serde_json::from_str(&std::fs::read_to_string(m["sidecar"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(side["start_sample"], n - 72_000);
    assert_eq!(side["samples"], 72_000);
    assert_eq!(side["reason"], "api");
    assert_eq!(side["dcd"][0]["t"], 0.0);
    assert_eq!(side["config"]["ring_seconds"], 2.0);

    // Default: the whole ring (2 s), and the name does not collide.
    let m2 = api.capture(&modem, None, Some("tone"), "control").unwrap();
    assert_ne!(m2["wav"], m["wav"]);
    let wav2 = cairn_phy::wav::read(std::path::Path::new(m2["wav"].as_str().unwrap())).unwrap();
    let want2: Vec<f64> = audio[n - 96_000..].iter().map(|&v| q(v)).collect();
    assert_eq!(wav2, want2);
    // More than the ring holds: what it holds.
    let m3 = api.capture(&modem, Some(10.0), None, "api").unwrap();
    assert_eq!(m3["samples"], 96_000);
    let _ = std::fs::remove_dir_all(&dir);
}
