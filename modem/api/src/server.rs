//! The API server: TCP and Unix-domain stream sockets, any number of
//! clients, JSON lines both ways (`docs/api.md` "Transport").
//!
//! Threads: one accept thread per listener; per client a reader thread
//! (lines in, at most [`MAX_LINE`] bytes each) and a writer thread fed by a
//! bounded queue, so a client that stops reading never blocks the modem: a
//! client whose queue is full is disconnected. Lines in arrive on one
//! channel as [`Inbound`]; the modem's event loop answers with
//! [`ApiServer::send`] and publishes events with [`ApiServer::publish`],
//! which honours each client's subscription. A new client receives nothing
//! until the loop has sent its `hello` ([`ApiServer::send_hello`]).

use std::collections::{BTreeSet, HashMap};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex};

use crate::proto::{EVENTS, MAX_LINE};

/// A client's number (1, 2, ...).
pub type ClientId = u64;

/// Lines queued for one client before it counts as stuck.
const QUEUE: usize = 4096;

/// From the server to the event loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inbound {
    /// A client connected (send its `hello`).
    Connected(ClientId),
    /// A request line (CR and the LF removed; blank lines are skipped).
    Line(ClientId, String),
    /// A line over [`MAX_LINE`] bytes was discarded.
    TooLong(ClientId),
    /// A line that is not UTF-8.
    NotUtf8(ClientId),
    /// The client went away.
    Disconnected(ClientId),
}

struct Client {
    tx: SyncSender<String>,
    ready: bool,
    /// None: every event.
    subs: Option<BTreeSet<String>>,
    close: Box<dyn Fn() + Send>,
}

/// The server.
#[derive(Clone)]
pub struct ApiServer {
    clients: Arc<Mutex<HashMap<ClientId, Client>>>,
    next: Arc<AtomicU64>,
    inbound: Sender<Inbound>,
}

/// A stream a client talks over.
trait Conn: Read + Write + Send + 'static {
    fn split(&self) -> io::Result<Box<dyn Conn>>;
    fn shutdown_both(&self);
}

impl Conn for TcpStream {
    fn split(&self) -> io::Result<Box<dyn Conn>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn shutdown_both(&self) {
        let _ = self.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(unix)]
impl Conn for std::os::unix::net::UnixStream {
    fn split(&self) -> io::Result<Box<dyn Conn>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn shutdown_both(&self) {
        let _ = self.shutdown(std::net::Shutdown::Both);
    }
}

impl ApiServer {
    /// A server with no listeners yet, and the channel its lines arrive on.
    pub fn new() -> (ApiServer, Receiver<Inbound>) {
        let (tx, rx) = channel();
        (ApiServer { clients: Arc::default(), next: Arc::new(AtomicU64::new(1)), inbound: tx }, rx)
    }

    /// Listen on a TCP address (`127.0.0.1:8002`; port 0 picks one).
    pub fn listen_tcp(&self, addr: &str) -> io::Result<SocketAddr> {
        let l = TcpListener::bind(addr)?;
        let local = l.local_addr()?;
        let me = self.clone();
        std::thread::Builder::new().name("cairn-api-tcp".into()).spawn(move || {
            for s in l.incoming() {
                match s {
                    Ok(s) => {
                        let _ = s.set_nodelay(true);
                        me.attach(Box::new(s));
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
                }
            }
        })?;
        Ok(local)
    }

    /// Listen on a Unix-domain socket at `path` (a stale socket there is
    /// replaced; any other file is an error).
    #[cfg(unix)]
    pub fn listen_unix(&self, path: &std::path::Path) -> io::Result<()> {
        use std::os::unix::fs::FileTypeExt;
        if let Ok(m) = std::fs::symlink_metadata(path) {
            if m.file_type().is_socket() {
                std::fs::remove_file(path)?;
            } else {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "exists and is not a socket"));
            }
        }
        let l = std::os::unix::net::UnixListener::bind(path)?;
        let me = self.clone();
        std::thread::Builder::new().name("cairn-api-unix".into()).spawn(move || {
            for s in l.incoming() {
                match s {
                    Ok(s) => me.attach(Box::new(s)),
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
                }
            }
        })?;
        Ok(())
    }

    fn attach(&self, conn: Box<dyn Conn>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (Ok(writer), Ok(closer)) = (conn.split(), conn.split()) else { return };
        let (tx, rx) = sync_channel::<String>(QUEUE);
        let closer: Box<dyn Fn() + Send> = Box::new(move || closer.shutdown_both());
        self.clients.lock().expect("clients").insert(id, Client { tx, ready: false, subs: None, close: closer });
        // Writer.
        std::thread::spawn(move || {
            let mut w = io::BufWriter::new(writer);
            while let Ok(line) = rx.recv() {
                let mut ok = w.write_all(line.as_bytes()).is_ok() && w.write_all(b"\n").is_ok();
                // Flush when nothing else is waiting.
                if ok {
                    while let Ok(more) = rx.try_recv() {
                        if w.write_all(more.as_bytes()).is_err() || w.write_all(b"\n").is_err() {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok || w.flush().is_err() {
                    w.get_ref().shutdown_both();
                    break;
                }
            }
        });
        // Reader.
        let inbound = self.inbound.clone();
        let clients = self.clients.clone();
        let _ = inbound.send(Inbound::Connected(id));
        std::thread::spawn(move || {
            read_lines(conn, id, &inbound);
            if let Some(c) = clients.lock().expect("clients").remove(&id) {
                (c.close)();
            }
            let _ = inbound.send(Inbound::Disconnected(id));
        });
    }

    /// Send a client its `hello`; from now on it receives events.
    pub fn send_hello(&self, id: ClientId, line: &str) {
        let mut c = self.clients.lock().expect("clients");
        if let Some(cl) = c.get_mut(&id) {
            cl.ready = true;
        }
        drop(c);
        self.send(id, line);
    }

    /// Send one line to one client (a response).
    pub fn send(&self, id: ClientId, line: &str) {
        let mut c = self.clients.lock().expect("clients");
        let stuck = match c.get(&id) {
            Some(cl) => matches!(cl.tx.try_send(line.to_string()), Err(TrySendError::Full(_))),
            None => false,
        };
        if stuck {
            if let Some(cl) = c.remove(&id) {
                (cl.close)();
            }
        }
    }

    /// Publish an event line to every ready client subscribed to `event`.
    pub fn publish(&self, event: &str, line: &str) {
        let mut c = self.clients.lock().expect("clients");
        let mut stuck = Vec::new();
        for (id, cl) in c.iter() {
            if !cl.ready || cl.subs.as_ref().is_some_and(|s| !s.contains(event)) {
                continue;
            }
            if let Err(TrySendError::Full(_)) = cl.tx.try_send(line.to_string()) {
                stuck.push(*id);
            }
        }
        for id in stuck {
            if let Some(cl) = c.remove(&id) {
                (cl.close)();
            }
        }
    }

    /// Set a client's subscription (None: every event). Returns the event
    /// names now in force.
    pub fn subscribe(&self, id: ClientId, events: Option<BTreeSet<String>>) -> Vec<String> {
        let mut c = self.clients.lock().expect("clients");
        let names = match &events {
            None => EVENTS.iter().map(|s| s.to_string()).collect(),
            Some(s) => EVENTS.iter().filter(|e| s.contains(**e)).map(|s| s.to_string()).collect(),
        };
        if let Some(cl) = c.get_mut(&id) {
            cl.subs = events;
        }
        names
    }

    /// Connected clients.
    pub fn client_count(&self) -> usize {
        self.clients.lock().expect("clients").len()
    }

    /// Disconnect every client.
    pub fn close_all(&self) {
        let mut c = self.clients.lock().expect("clients");
        for (_, cl) in c.drain() {
            (cl.close)();
        }
    }
}

/// Read LF-terminated lines of at most [`MAX_LINE`] bytes until EOF.
fn read_lines(conn: Box<dyn Conn>, id: ClientId, inbound: &Sender<Inbound>) {
    let mut r = BufReader::with_capacity(64 * 1024, conn);
    let mut line: Vec<u8> = Vec::new();
    let mut discarding = false;
    loop {
        let buf = match r.fill_buf() {
            Ok([]) => return,
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let (take, end) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i, true),
            None => (buf.len(), false),
        };
        if !discarding {
            if line.len() + take > MAX_LINE {
                discarding = true;
                line.clear();
            } else {
                line.extend_from_slice(&buf[..take]);
            }
        }
        r.consume(take + end as usize);
        if end {
            let msg = if discarding {
                Some(Inbound::TooLong(id))
            } else {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                match std::str::from_utf8(&line) {
                    Ok(s) if s.trim().is_empty() => None,
                    Ok(s) => Some(Inbound::Line(id, s.to_string())),
                    Err(_) => Some(Inbound::NotUtf8(id)),
                }
            };
            discarding = false;
            line.clear();
            if let Some(m) = msg {
                if inbound.send(m).is_err() {
                    return;
                }
            }
        }
    }
}
