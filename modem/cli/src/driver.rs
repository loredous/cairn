//! The live modem (`cairn-modem run`): the [`Modem`] core on a sound card
//! and a PTT, with the KISS server, the native API (`hostapi`, SPEC 13.2)
//! and the line control interface.
//!
//! Threads: this one owns the core and the capture stream and runs the
//! event loop; a transmit thread owns the playback stream and the PTT and
//! runs each keyed transmission (`txseq::transmit`: key, TXDelay, audio,
//! drain, TXTail, unkey), reporting the measured release time; the KISS
//! server, the API server and the control readers send commands over
//! channels. The
//! station's clock is the monotonic clock since start; capture sample times
//! come from a running estimate of when each sample arrived (the earliest
//! arrival over the last few seconds, which tracks drift between the
//! sound card and the system clock).

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver as ChanRx, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cairn_audio::{InputStream, OutputPort, OutputStream, StreamOptions};
use cairn_link::pdu::pdu_type;
use cairn_link::{HostCommand, Instant};
use cairn_phy::params::FS;
use cairn_ptt::SharedPtt;

use cairn_api::proto::{ApiError, Code, Rejected};
use cairn_api::server::{ApiServer, Inbound};
use serde_json::Value;

use crate::control::{self, Control, HostState};
use crate::hostapi::{HostApi, HostApiConfig};
use crate::kiss::{self, KissCommand};
use crate::runtime::{DecodeTime, Modem, ModemConfig, Output};
use crate::txseq::{self, TxTiming};

/// Samples per chunk.
const CHUNK: usize = 480;

/// The run's start, for the time stamp on every output line.
static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Live-modem options.
#[derive(Clone, Debug)]
pub struct RunOptions {
    /// Audio device query.
    pub device: String,
    /// Stream options.
    pub stream: StreamOptions,
    /// KISS TCP address (None: no KISS server).
    pub kiss: Option<String>,
    /// Control TCP address (line protocol, as stdin).
    pub control: Option<String>,
    /// Read control lines from stdin.
    pub stdin: bool,
    /// Stop after this long.
    pub seconds: Option<f64>,
    /// Refuse to key once this much keyed time has been used.
    pub tx_budget: Option<Duration>,
    /// Print every decoded header and DCD change.
    pub verbose: bool,
    /// Status line interval (None: never).
    pub status_every: Option<Duration>,
    /// Save received files and messages here.
    pub rx_dir: Option<std::path::PathBuf>,
    /// Native API on TCP (None: off).
    pub api: Option<String>,
    /// Native API on a Unix socket.
    pub api_socket: Option<std::path::PathBuf>,
    /// Where captures go.
    pub capture_dir: std::path::PathBuf,
    /// Keep the heard list in this file.
    pub heard_file: Option<std::path::PathBuf>,
    /// Capture automatically after a frame from the session peer fails
    /// (a header failure, or data blocks lost), at most once a minute.
    pub capture_on_failure: bool,
}

/// What the event loop hears from other threads.
enum Msg {
    Control(String),
    Kiss(SocketAddr, KissCommand),
    Api(Inbound),
    TxDone { release_wall: std::time::Instant, keyed_ms: f64, txdelay_ms: f64, txtail_ms: f64, err: Option<String> },
}

/// The earliest-arrival estimate of capture sample times.
struct SampleClock {
    window: VecDeque<(f64, f64)>,
}

impl SampleClock {
    /// `end` samples captured by wall time `w` (s): note it, return the
    /// estimated wall time (s) of sample 0.
    fn note(&mut self, w: f64, end: usize) -> f64 {
        let o = w - end as f64 / FS as f64;
        self.window.push_back((w, o));
        while self.window.front().is_some_and(|(t, _)| *t < w - 5.0) {
            self.window.pop_front();
        }
        self.window.iter().map(|(_, o)| *o).fold(f64::INFINITY, f64::min)
    }
}

fn output_line(line: &str, clients: &Arc<Mutex<Vec<TcpStream>>>) {
    if line.is_empty() {
        return;
    }
    let t = START.get().map_or(0.0, |s| s.elapsed().as_secs_f64());
    let line = format!("[{t:9.3}] {line}");
    println!("{line}");
    let _ = std::io::stdout().flush();
    let mut c = clients.lock().expect("clients");
    c.retain_mut(|s| writeln!(s, "{line}").is_ok());
}

/// Run until stopped; returns the final counters line.
pub fn run(
    mut cfg: ModemConfig,
    opts: &RunOptions,
    ptt: SharedPtt,
    stop: &'static AtomicBool,
) -> Result<String, String> {
    let origin = std::time::Instant::now();
    let _ = START.set(origin);
    let now = move || Instant(origin.elapsed().as_micros() as u64);
    cfg.decode_time = DecodeTime::Wall(origin);
    let ring_seconds = cfg.ring_seconds;
    let mut modem = Modem::new(cfg);
    let mut host = HostState::new();
    let epoch = cairn_api::proto::now_unix_us();
    let mut api = HostApi::new(
        HostApiConfig {
            capture_dir: opts.capture_dir.clone(),
            heard_file: opts.heard_file.clone(),
            epoch_unix_us: epoch,
        },
        ring_seconds,
    )?;
    let mut rx_dir = match &opts.rx_dir {
        Some(d) => {
            let r = crate::rxdir::RxDir::new(d).map_err(|e| format!("--rx-dir {}: {e}", d.display()))?;
            println!("received files and messages go to {}", d.display());
            Some(r)
        }
        None => None,
    };
    let (msg_tx, msg_rx): (Sender<Msg>, ChanRx<Msg>) = channel();
    let clients: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));

    let mut input = InputStream::open(&opts.device, &opts.stream).map_err(|e| e.to_string())?;
    println!("input {} ({})", input.id, input.format);

    // Transmit thread: the playback stream and the PTT.
    let (job_tx, job_rx) = channel::<(Vec<f32>, TxTiming)>();
    let device = opts.device.clone();
    let stream_opts = opts.stream.clone();
    let tx_msgs = msg_tx.clone();
    let (ready_tx, ready_rx) = channel::<Result<String, String>>();
    let ptt_tx = ptt.clone();
    let tx_thread = std::thread::Builder::new()
        .name("cairn-tx".into())
        .spawn(move || {
            let mut out = match OutputStream::open(&device, &stream_opts) {
                Ok(o) => o,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            if !out.wait_running(Duration::from_secs(2)) {
                let _ = ready_tx.send(Err("the output device did not start".into()));
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
            let _ = ready_tx.send(Ok(format!(
                "output {} ({}), latency {:.1} ms",
                out.id,
                out.format,
                out.latency_ns() as f64 / 1e6
            )));
            for (audio, timing) in job_rx {
                let r = txseq::transmit(&mut out, &ptt_tx, &audio, &timing, stop);
                let port_now = out.now_ns();
                let wall_now = std::time::Instant::now();
                let msg = match r {
                    Ok(rep) => Msg::TxDone {
                        release_wall: wall_now - Duration::from_nanos(port_now.saturating_sub(rep.unkey_ns)),
                        keyed_ms: rep.keyed_ms(),
                        txdelay_ms: rep.txdelay_ms(),
                        txtail_ms: rep.txtail_ms(),
                        err: None,
                    },
                    Err(e) => Msg::TxDone {
                        release_wall: wall_now,
                        keyed_ms: 0.0,
                        txdelay_ms: 0.0,
                        txtail_ms: 0.0,
                        err: Some(e.to_string()),
                    },
                };
                if tx_msgs.send(msg).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| e.to_string())?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(line)) => println!("{line}"),
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("the output device did not open".into()),
    }
    println!("PTT {}", ptt.describe());

    // KISS server.
    let hub = match &opts.kiss {
        Some(addr) => {
            let (ktx, krx) = channel();
            let (hub, local, _t) = kiss::serve(addr, ktx).map_err(|e| format!("KISS on {addr}: {e}"))?;
            println!("KISS server on {local}");
            let m = msg_tx.clone();
            std::thread::spawn(move || {
                for (peer, c) in krx {
                    if m.send(Msg::Kiss(peer, c)).is_err() {
                        break;
                    }
                }
            });
            Some(hub)
        }
        None => None,
    };
    // Native API.
    let server = if opts.api.is_some() || opts.api_socket.is_some() {
        let (srv, inbound) = ApiServer::new();
        if let Some(addr) = &opts.api {
            let local = srv.listen_tcp(addr).map_err(|e| format!("API on {addr}: {e}"))?;
            println!("native API on {local}");
        }
        #[cfg(unix)]
        if let Some(p) = &opts.api_socket {
            srv.listen_unix(p).map_err(|e| format!("API socket {}: {e}", p.display()))?;
            println!("native API on {}", p.display());
        }
        let m = msg_tx.clone();
        std::thread::spawn(move || {
            for i in inbound {
                if m.send(Msg::Api(i)).is_err() {
                    break;
                }
            }
        });
        Some(srv)
    } else {
        None
    };
    let publish = |api: &mut HostApi, srv: &Option<ApiServer>| {
        for e in api.take_events() {
            if let Some(s) = srv {
                s.publish(e.name, &e.line);
            }
        }
    };
    let mut auto_capture: Option<std::time::Instant> = None;
    let mut last_auto: Option<std::time::Instant> = None;
    // Control: stdin and a TCP port.
    if opts.stdin {
        let m = msg_tx.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if m.send(Msg::Control(line)).is_err() {
                    break;
                }
            }
        });
    }
    if let Some(addr) = &opts.control {
        let l = TcpListener::bind(addr).map_err(|e| format!("control on {addr}: {e}"))?;
        println!("control on {}", l.local_addr().map_err(|e| e.to_string())?);
        let m = msg_tx.clone();
        let cl = clients.clone();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                if let Ok(w) = s.try_clone() {
                    cl.lock().expect("clients").push(w);
                }
                let m = m.clone();
                std::thread::spawn(move || {
                    for line in std::io::BufReader::new(s).lines() {
                        let Ok(line) = line else { break };
                        if m.send(Msg::Control(line)).is_err() {
                            break;
                        }
                    }
                });
            }
        });
    }
    drop(msg_tx);
    println!("{} listening; {}", modem.station().address(), control::HELP);

    let mut clock = SampleClock { window: VecDeque::new() };
    let mut taken = 0usize;
    let mut buf = vec![0f32; CHUNK];
    let mut chunk: Vec<f64> = Vec::with_capacity(CHUNK);
    let mut keyed_total = Duration::ZERO;
    let mut next_status = opts.status_every.map(|d| origin + d);
    let mut quit = false;
    // KISS frames' host ids, apart from the control interface's.
    let mut kiss_id: u64 = 1 << 40;
    let mut err = None;
    while !quit && !stop.load(Ordering::Relaxed) {
        if opts.seconds.is_some_and(|s| origin.elapsed().as_secs_f64() >= s) {
            break;
        }
        // Messages.
        while let Ok(m) = msg_rx.try_recv() {
            match m {
                Msg::Api(i) => {
                    let Some(srv) = &server else { continue };
                    match i {
                        Inbound::Connected(id) => srv.send_hello(id, &api.hello(id, &modem, now())),
                        Inbound::Line(id, line) => {
                            let h = api.request(&line, &mut modem, now());
                            if let Some(sub) = h.subscribe {
                                srv.subscribe(id, sub);
                            }
                            srv.send(id, &h.response);
                        }
                        Inbound::TooLong(id) | Inbound::NotUtf8(id) => {
                            let error = match i {
                                Inbound::TooLong(_) => ApiError::new(Code::LineTooLong, "request line over 1 MiB"),
                                _ => ApiError::new(Code::BadJson, "request line is not UTF-8"),
                            };
                            srv.send(id, &Rejected { id: Value::Null, cmd: Value::Null, error }.response().to_string());
                        }
                        Inbound::Disconnected(_) => {}
                    }
                }
                Msg::Control(line) => match control::parse(&line) {
                    Ok(Control::Quit) => quit = true,
                    Ok(Control::Status) => output_line(&control::status(&modem), &clients),
                    Ok(Control::Heard) => {
                        let list = api.heard().list(None, None);
                        if list.is_empty() {
                            output_line("heard: nobody yet", &clients);
                        }
                        for e in list {
                            output_line(
                                &format!(
                                    "heard {} last {} ({} headers, last {} MCS {} x{} SNR {:.1} dB, best {:.1} dB)",
                                    e.address,
                                    cairn_api::proto::utc_string(e.last_us),
                                    e.count,
                                    e.last_frame_type,
                                    e.last_mcs,
                                    e.last_width,
                                    e.last_snr_db,
                                    e.best_snr_db
                                ),
                                &clients,
                            );
                        }
                    }
                    Ok(Control::Capture { seconds, label }) => {
                        match api.capture(&modem, seconds, label.as_deref(), "control") {
                            Ok(m) => output_line(
                                &format!(
                                    "capture: {} s -> {} and {}",
                                    m["seconds"],
                                    m["wav"].as_str().unwrap_or(""),
                                    m["sidecar"].as_str().unwrap_or("")
                                ),
                                &clients,
                            ),
                            Err(e) => output_line(&format!("error: capture: {}", e.message), &clients),
                        }
                    }
                    Ok(Control::Help) => output_line(control::HELP, &clients),
                    Ok(c) => match host.commands(&c) {
                        Ok((cmds, line)) => {
                            output_line(&line, &clients);
                            for cmd in cmds {
                                modem.host(cmd, now());
                            }
                        }
                        Err(e) => output_line(&format!("error: {e}"), &clients),
                    },
                    Err(e) if e.is_empty() => {}
                    Err(e) => output_line(&format!("error: {e}"), &clients),
                },
                Msg::Kiss(peer, c) => match c {
                    KissCommand::Data(frame) => {
                        kiss_id += 1;
                        let id = kiss_id;
                        if opts.verbose {
                            output_line(&format!("KISS {peer}: AX.25 frame, {} bytes", frame.len()), &clients);
                        }
                        modem.host(HostCommand::Broadcast { id, pdu: kiss::ax25_pdu(frame), repeats: 0 }, now());
                    }
                    KissCommand::TxDelay(v) => {
                        let txtail = modem.config().txtail;
                        modem.set_timing(Duration::from_millis(10 * v as u64), txtail);
                        output_line(&format!("KISS {peer}: TXDELAY {} ms", 10 * v as u64), &clients);
                    }
                    KissCommand::Persistence(p) => {
                        modem.station_mut().config_mut().persistence = (p as f64 + 1.0) / 256.0;
                        output_line(&format!("KISS {peer}: persistence p = {:.3}", (p as f64 + 1.0) / 256.0), &clients);
                    }
                    KissCommand::SlotTime(v) => {
                        modem.station_mut().config_mut().slot_time = Duration::from_millis(10 * v as u64);
                        output_line(&format!("KISS {peer}: slot time {} ms", 10 * v as u64), &clients);
                    }
                    KissCommand::TxTail(v) => {
                        let txdelay = modem.config().txdelay;
                        modem.set_timing(txdelay, Duration::from_millis(10 * v as u64));
                    }
                    KissCommand::FullDuplex(_) | KissCommand::SetHardware(_) | KissCommand::Return => {}
                },
                Msg::TxDone { release_wall, keyed_ms, txdelay_ms, txtail_ms, err: e } => {
                    let release = Instant(release_wall.saturating_duration_since(origin).as_micros() as u64);
                    keyed_total += Duration::from_secs_f64(keyed_ms / 1e3);
                    match e {
                        None => {
                            output_line(
                                &format!(
                                    "tx done: keyed {keyed_ms:.0} ms, TXDelay {txdelay_ms:.1} ms, TXTail {txtail_ms:.1} ms (at the DAC)"
                                ),
                                &clients,
                            );
                            api.on_tx_done(Some((keyed_ms, txdelay_ms, txtail_ms)), now());
                        }
                        Some(e) => {
                            output_line(&format!("tx failed: {e} (PTT released)"), &clients);
                            api.on_tx_failed(&format!("transmission failed: {e} (PTT released)"), now());
                        }
                    }
                    modem.tx_done(release, now());
                }
            }
        }
        // Audio.
        loop {
            let avail = input.available();
            if avail < CHUNK - chunk.len() {
                break;
            }
            let want = CHUNK - chunk.len();
            match input.read(&mut buf[..want]) {
                Ok(n) => chunk.extend(buf[..n].iter().map(|&v| v as f64)),
                Err(e) => {
                    err = Some(e.to_string());
                    break;
                }
            }
            if chunk.len() == CHUNK {
                let w = origin.elapsed().as_secs_f64();
                let zero = clock.note(w, taken + CHUNK + input.available());
                let t_first = Instant(((zero + taken as f64 / FS as f64).max(0.0) * 1e6) as u64);
                modem.push_audio(&chunk, t_first, now());
                taken += CHUNK;
                chunk.clear();
            }
        }
        if err.is_some() {
            break;
        }
        if modem.next_wakeup().is_some_and(|w| w <= now()) {
            modem.tick(now());
        }
        // Outputs.
        for o in modem.drain() {
            let mut saved = None;
            if let Some(r) = rx_dir.as_mut() {
                let res = match &o {
                    Output::Deliver { from, pdu } => Some((*from, pdu, r.save(*from, pdu, false))),
                    Output::DeliverBroadcast { from, ftype: cairn_link::FrameType::Ui, pdu } => {
                        Some((*from, pdu, r.save(*from, pdu, true)))
                    }
                    _ => None,
                };
                match res {
                    Some((from, pdu, Ok(Some(s)))) => {
                        s.lines(from, pdu).iter().for_each(|l| output_line(l, &clients));
                        saved = Some(s);
                    }
                    Some((_, _, Err(e))) => output_line(&format!("error: --rx-dir: {e}"), &clients),
                    _ => {}
                }
            }
            api.on_output(&o, saved.as_ref(), &modem, now());
            if opts.capture_on_failure && auto_capture.is_none() {
                let me = modem.station().address();
                let peer = modem.station().session_info().map(|i| i.peer);
                let failed = match &o {
                    Output::HeaderFailed { .. } => peer.is_some(),
                    Output::Heard(f) => f.header.dst == me && Some(f.header.src) == peer && f.blocks_ok < f.blocks,
                    _ => false,
                };
                let quiet = last_auto.is_none_or(|t| t.elapsed() >= Duration::from_secs(60));
                if failed && quiet {
                    // A little after the failure, so the capture holds what followed.
                    auto_capture = Some(std::time::Instant::now() + Duration::from_secs(2));
                }
            }
            match &o {
                Output::Transmit(req) => {
                    let keyed = req.keyed(modem.config().txdelay, modem.config().txtail);
                    if opts.tx_budget.is_some_and(|b| keyed_total + keyed > b) {
                        output_line(
                            &format!("tx budget exhausted: {:?} not keyed (PTT stays released)", req.frame.ftype),
                            &clients,
                        );
                        api.on_tx_failed("tx budget exhausted: not keyed", now());
                        modem.tx_done(now(), now());
                        continue;
                    }
                    output_line(
                        &format!(
                            "tx {:?} to {} seq {} MCS {} ctl {} data {}{} ({:.2} s keyed)",
                            req.frame.ftype,
                            req.frame.dst,
                            req.frame.seq.0,
                            req.frame.mcs,
                            req.frame.controls.len(),
                            req.frame.data.len(),
                            if req.turn_holder { " turn-holder" } else { "" },
                            keyed.as_secs_f64()
                        ),
                        &clients,
                    );
                    let timing = TxTiming {
                        txdelay_s: modem.config().txdelay.as_secs_f64(),
                        txtail_s: modem.config().txtail.as_secs_f64(),
                    };
                    if job_tx.send((req.audio.clone(), timing)).is_err() {
                        err = Some("the transmit thread stopped".into());
                    }
                }
                Output::DeliverBroadcast { pdu, .. } if pdu.pdu_type == pdu_type::AX25 => {
                    if let Some(h) = &hub {
                        h.send(&pdu.body);
                    }
                    if let Some(l) = host.describe(&o) {
                        output_line(&l, &clients);
                    }
                }
                Output::Heard(f) if opts.verbose => output_line(&format!("rx {}", f.line()), &clients),
                Output::Dcd { at, state } if opts.verbose => {
                    output_line(&format!("dcd {:.3} {state:?}", at.as_secs_f64()), &clients)
                }
                Output::HeaderFailed { .. } if opts.verbose => output_line("rx preamble, header failed", &clients),
                Output::Log(l) => output_line(&format!("note: {l}"), &clients),
                _ => {
                    if let Some(l) = host.describe(&o) {
                        output_line(&l, &clients);
                    }
                }
            }
        }
        api.poll(&modem, now());
        if auto_capture.is_some_and(|t| std::time::Instant::now() >= t) {
            auto_capture = None;
            last_auto = Some(std::time::Instant::now());
            match api.capture(&modem, None, Some("auto"), "auto") {
                Ok(m) => output_line(&format!("auto capture after a failed frame: {}", m["wav"]), &clients),
                Err(e) => output_line(&format!("error: auto capture: {}", e.message), &clients),
            }
        }
        publish(&mut api, &server);
        if let Some(t) = next_status {
            if std::time::Instant::now() >= t {
                output_line(&control::status(&modem), &clients);
                next_status = Some(t + opts.status_every.expect("interval"));
            }
        }
        let sleep = modem
            .next_wakeup()
            .map(|w| Duration::from_micros(w.0.saturating_sub(now().0)))
            .unwrap_or(Duration::from_millis(2))
            .min(Duration::from_millis(2));
        std::thread::sleep(sleep.max(Duration::from_micros(200)));
    }
    drop(job_tx);
    let _ = tx_thread.join();
    if let Err(e) = api.save_heard() {
        eprintln!("error: {e}");
    }
    if let Some(s) = &server {
        s.close_all();
    }
    #[cfg(unix)]
    if let Some(p) = &opts.api_socket {
        let _ = std::fs::remove_file(p);
    }
    if let Ok(Some(true)) = ptt.read_back() {
        return Err("PTT still reads keyed after release".into());
    }
    let s = input.stats();
    let summary = format!(
        "{}\ncapture: xruns {} overflows {} gaps {}; decode p50 {:.1} ms",
        control::status(&modem),
        s.xruns,
        s.overflow_events,
        s.gaps,
        median(&modem.stats.decode_ms)
    );
    if let Some(e) = err {
        return Err(format!("{e}\n{summary}"));
    }
    Ok(summary)
}

fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    s[s.len() / 2]
}
