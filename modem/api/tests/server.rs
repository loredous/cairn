//! The API server over real sockets: hello first, line framing (CRLF,
//! blank lines, over-long and non-UTF-8 lines), subscriptions, several
//! clients, TCP and a Unix socket.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use cairn_api::proto::MAX_LINE;
use cairn_api::server::{ApiServer, Inbound};

fn next(rx: &Receiver<Inbound>) -> Inbound {
    rx.recv_timeout(Duration::from_secs(5)).expect("inbound")
}

fn line(r: &mut impl BufRead) -> String {
    let mut s = String::new();
    r.read_line(&mut s).expect("read");
    s.trim_end().to_string()
}

#[test]
fn tcp_framing_hello_and_subscriptions() {
    let (srv, rx) = ApiServer::new();
    let addr = srv.listen_tcp("127.0.0.1:0").unwrap();
    let a = TcpStream::connect(addr).unwrap();
    a.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut ar = BufReader::new(a.try_clone().unwrap());
    let mut aw = a;
    let Inbound::Connected(ida) = next(&rx) else { panic!("connected") };
    // Nothing reaches a client before its hello.
    srv.publish("dcd", r#"{"event":"dcd","early":true}"#);
    srv.send_hello(ida, r#"{"event":"hello"}"#);
    assert_eq!(line(&mut ar), r#"{"event":"hello"}"#);

    // Framing.
    aw.write_all(b"{\"cmd\":\"status\"}\r\n\n   \n{\"cmd\":\"heard\"}\n").unwrap();
    assert_eq!(next(&rx), Inbound::Line(ida, r#"{"cmd":"status"}"#.into()));
    assert_eq!(next(&rx), Inbound::Line(ida, r#"{"cmd":"heard"}"#.into()));
    let mut big = vec![b'x'; MAX_LINE + 10];
    big.push(b'\n');
    aw.write_all(&big).unwrap();
    aw.write_all(b"\xff\xfe\n{\"cmd\":\"status\",\"id\":2}\n").unwrap();
    assert_eq!(next(&rx), Inbound::TooLong(ida));
    assert_eq!(next(&rx), Inbound::NotUtf8(ida));
    assert_eq!(next(&rx), Inbound::Line(ida, r#"{"cmd":"status","id":2}"#.into()));
    // Exactly MAX_LINE is fine.
    let mut fits = vec![b' '; MAX_LINE - 2];
    fits[0] = b'{';
    fits[MAX_LINE - 3] = b'}';
    fits.push(b'\n');
    aw.write_all(&fits).unwrap();
    let Inbound::Line(_, l) = next(&rx) else { panic!("line") };
    assert_eq!(l.len(), MAX_LINE - 2);

    // A second client, subscribed to heard only.
    let b = TcpStream::connect(addr).unwrap();
    b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut br = BufReader::new(b.try_clone().unwrap());
    let Inbound::Connected(idb) = next(&rx) else { panic!("connected") };
    assert_ne!(ida, idb);
    srv.send_hello(idb, r#"{"event":"hello","client":2}"#);
    assert_eq!(line(&mut br), r#"{"event":"hello","client":2}"#);
    let names = srv.subscribe(idb, Some(["heard".to_string()].into_iter().collect()));
    assert_eq!(names, ["heard"]);
    srv.publish("dcd", r#"{"event":"dcd"}"#);
    srv.publish("heard", r#"{"event":"heard"}"#);
    srv.send(idb, r#"{"id":1,"ok":true}"#);
    assert_eq!(line(&mut ar), r#"{"event":"dcd"}"#);
    assert_eq!(line(&mut ar), r#"{"event":"heard"}"#);
    assert_eq!(line(&mut br), r#"{"event":"heard"}"#);
    assert_eq!(line(&mut br), r#"{"id":1,"ok":true}"#);
    assert_eq!(srv.client_count(), 2);

    drop(br);
    drop(b);
    assert_eq!(next(&rx), Inbound::Disconnected(idb));
    srv.close_all();
    assert_eq!(next(&rx), Inbound::Disconnected(ida));
}

#[cfg(unix)]
#[test]
fn unix_socket() {
    use std::os::unix::net::UnixStream;
    let dir = std::env::temp_dir().join(format!("cairn-api-sock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("api.sock");
    let (srv, rx) = ApiServer::new();
    srv.listen_unix(&path).unwrap();
    let s = UnixStream::connect(&path).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut w = s;
    let Inbound::Connected(id) = next(&rx) else { panic!("connected") };
    srv.send_hello(id, r#"{"event":"hello"}"#);
    assert_eq!(line(&mut r), r#"{"event":"hello"}"#);
    w.write_all(b"{\"cmd\":\"status\"}\n").unwrap();
    assert_eq!(next(&rx), Inbound::Line(id, r#"{"cmd":"status"}"#.into()));
    // A second server on the same path replaces the stale socket; a
    // regular file there is refused.
    let (srv2, _rx2) = ApiServer::new();
    srv2.listen_unix(&path).unwrap();
    let f = dir.join("file");
    std::fs::write(&f, "x").unwrap();
    assert!(srv2.listen_unix(&f).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
