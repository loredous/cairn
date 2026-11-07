//! Hamlib `rigctld` keying over TCP: `T 1` / `T 0`, answered by `RPRT 0`.
//! Its latency (CAT round trip plus the radio's own switching) belongs in
//! TXDelay (SPEC 13.3).

use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::Ptt;

const TIMEOUT: Duration = Duration::from_secs(2);

/// rigctld keying.
pub struct RigctldPtt {
    reader: BufReader<TcpStream>,
    addr: String,
}

impl RigctldPtt {
    /// Connect to `addr` (host:port) and unkey.
    pub fn connect(addr: &str) -> io::Result<Self> {
        let sa = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("cannot resolve {addr}")))?;
        let s = TcpStream::connect_timeout(&sa, TIMEOUT)
            .map_err(|e| io::Error::new(e.kind(), format!("rigctld {addr}: {e}")))?;
        s.set_read_timeout(Some(TIMEOUT))?;
        s.set_write_timeout(Some(TIMEOUT))?;
        s.set_nodelay(true)?;
        let mut p = RigctldPtt { reader: BufReader::new(s), addr: addr.to_string() };
        p.set(false)?;
        Ok(p)
    }

    fn command(&mut self, cmd: &str) -> io::Result<String> {
        let s = self.reader.get_mut();
        s.write_all(cmd.as_bytes())?;
        s.write_all(b"\n")?;
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "rigctld closed the connection"));
        }
        Ok(line.trim().to_string())
    }
}

impl Ptt for RigctldPtt {
    fn set(&mut self, keyed: bool) -> io::Result<()> {
        let reply = self.command(if keyed { "T 1" } else { "T 0" })?;
        match reply.strip_prefix("RPRT ") {
            Some("0") => Ok(()),
            _ => Err(io::Error::other(format!("rigctld answered {reply:?} to PTT {}", u8::from(keyed)))),
        }
    }

    fn read_back(&mut self) -> io::Result<Option<bool>> {
        let reply = self.command("t")?;
        match reply.as_str() {
            "0" => Ok(Some(false)),
            "1" | "2" | "3" => Ok(Some(true)),
            _ => Ok(None),
        }
    }

    fn describe(&self) -> String {
        format!("rigctld {}", self.addr)
    }
}

impl Drop for RigctldPtt {
    fn drop(&mut self) {
        let _ = self.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[test]
    fn talks_rigctld_protocol() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        let server = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            let mut state = 0;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let cmd = line.trim().to_string();
                log2.lock().unwrap().push(cmd.clone());
                let reply = match cmd.as_str() {
                    "T 1" => {
                        state = 1;
                        "RPRT 0".to_string()
                    }
                    "T 0" => {
                        state = 0;
                        "RPRT 0".to_string()
                    }
                    "t" => state.to_string(),
                    _ => "RPRT -1".to_string(),
                };
                writeln!(w, "{reply}").unwrap();
            }
        });
        {
            let mut p = RigctldPtt::connect(&addr).unwrap();
            p.set(true).unwrap();
            assert_eq!(p.read_back().unwrap(), Some(true));
            p.set(false).unwrap();
            p.set(true).unwrap();
        } // drop unkeys
        server.join().unwrap();
        assert_eq!(*log.lock().unwrap(), ["T 0", "T 1", "t", "T 0", "T 1", "T 0"]);
    }
}
