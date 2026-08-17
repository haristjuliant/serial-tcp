//! The web layer: the token gate, the CSRF check, and the JSON contract.
//!
//! Driven over a real socket rather than by calling handlers directly, so the
//! headers and status codes the browser actually depends on are what gets
//! asserted. No serial hardware and no pseudo-terminals, so this runs on every
//! platform.

mod common;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use common::{TempDir, fake_pair};
use serialport::SerialPort as _;

use serial_tcp::dashboard::config::Config;
use serial_tcp::dashboard::http::{self, Assets};
use serial_tcp::dashboard::registry::{DeviceOpener, Registry};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Dashboard {
    addr: SocketAddr,
    registry: Arc<Registry>,
    _dir: TempDir,
}

fn start() -> Dashboard {
    let dir = TempDir::new("http");
    let (template, _device) = fake_pair();
    // Leak the device end: nothing in these tests reads it, but the line has to
    // outlive them or every clone would fail.
    std::mem::forget(_device);

    let opener: DeviceOpener = Arc::new(move |_path, _settings| Ok(template.try_clone()?));

    let config = Config::new(4001, TOKEN.to_owned());
    let registry = Registry::new(&config, dir.join("serial-tcp.json"), opener);

    let server = http::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let addr = server.server_addr().to_ip().expect("an IP listener");

    let serving = Arc::clone(&registry);
    std::thread::spawn(move || {
        let _ = http::serve(server, serving, Assets::new(None));
    });

    Dashboard {
        addr,
        registry,
        _dir: dir,
    }
}

struct Reply {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}

impl Reply {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// A one-shot HTTP/1.1 request. `Connection: close` keeps reading the response
/// simple — the server hangs up rather than waiting for another request.
fn send(addr: SocketAddr, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> Reply {
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in extra {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);

    let mut stream = TcpStream::connect(addr).expect("connect to the dashboard");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();

    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);

    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();

    Reply {
        status,
        headers,
        body: body.to_owned(),
    }
}

/// The headers a browser sends once it holds the cookie.
fn authed() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "Cookie",
            concat!(
                "st=",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            ),
        ),
        ("X-Requested-With", "serial-tcp"),
    ]
}

// ---------------------------------------------------------------------- auth

#[test]
fn the_api_is_closed_without_a_token() {
    let d = start();
    let reply = send(d.addr, "GET", "/api/ports", &[], "");
    assert_eq!(reply.status, 401);
}

#[test]
fn the_page_is_closed_without_a_token() {
    let d = start();
    let reply = send(d.addr, "GET", "/", &[], "");
    assert_eq!(reply.status, 401);
    assert!(reply.body.contains("token"), "should say what is missing");
}

#[test]
fn a_wrong_token_is_refused() {
    let d = start();
    let reply = send(d.addr, "GET", "/?token=nope", &[], "");
    assert_eq!(reply.status, 401);
    assert!(
        reply.header("set-cookie").is_none(),
        "no cookie for a bad token"
    );
}

#[test]
fn a_wrong_cookie_is_refused() {
    let d = start();
    let reply = send(d.addr, "GET", "/api/ports", &[("Cookie", "st=wrong")], "");
    assert_eq!(reply.status, 401);
}

/// EventSource cannot set headers, so the token has to become a cookie for the
/// live streams to work at all.
#[test]
fn the_right_token_in_the_url_sets_a_cookie_and_redirects() {
    let d = start();
    let reply = send(d.addr, "GET", &format!("/?token={TOKEN}"), &[], "");

    assert_eq!(reply.status, 302);
    assert_eq!(reply.header("location"), Some("/"));

    let cookie = reply.header("set-cookie").expect("a cookie should be set");
    assert!(cookie.contains(&format!("st={TOKEN}")));
    assert!(cookie.contains("HttpOnly"), "got: {cookie}");
    assert!(cookie.contains("SameSite=Strict"), "got: {cookie}");
    assert!(cookie.contains("Path=/"), "got: {cookie}");
}

#[test]
fn the_page_is_served_with_the_cookie() {
    let d = start();
    let reply = send(
        d.addr,
        "GET",
        "/",
        &[("Cookie", &format!("st={TOKEN}"))],
        "",
    );

    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("<title>serial-tcp</title>"));
    assert_eq!(reply.header("cache-control"), Some("no-store"));
}

/// A cookie alone would let another site drive the dashboard through the user's
/// browser, so mutating routes need a header no cross-origin request can set.
#[test]
fn mutating_requests_need_more_than_the_cookie() {
    let d = start();
    let reply = send(
        d.addr,
        "POST",
        "/api/ports",
        &[("Cookie", &format!("st={TOKEN}"))],
        r#"{"device":"FAKE0"}"#,
    );

    assert_eq!(reply.status, 403);
    assert!(
        reply.body.contains("X-Requested-With"),
        "got: {}",
        reply.body
    );
}

#[test]
fn a_foreign_origin_is_refused() {
    let d = start();
    let mut headers = authed();
    headers.push(("Origin", "http://evil.example"));

    let reply = send(
        d.addr,
        "POST",
        "/api/ports",
        &headers,
        r#"{"device":"FAKE0"}"#,
    );
    assert_eq!(reply.status, 403);
}

#[test]
fn reads_do_not_need_the_csrf_header() {
    let d = start();
    let reply = send(
        d.addr,
        "GET",
        "/api/ports",
        &[("Cookie", &format!("st={TOKEN}"))],
        "",
    );
    assert_eq!(reply.status, 200);
}

// ------------------------------------------------------------------- routing

#[test]
fn unknown_routes_are_not_found() {
    let d = start();
    let reply = send(d.addr, "GET", "/api/nonsense", &authed(), "");
    assert_eq!(reply.status, 404);
    assert_eq!(reply.json()["error"], "no such endpoint");
}

#[test]
fn an_unknown_port_id_is_not_found() {
    let d = start();
    let reply = send(d.addr, "POST", "/api/ports/ghost/start", &authed(), "");
    assert_eq!(reply.status, 404);
}

// ------------------------------------------------------------- the port cycle

#[test]
fn ports_start_out_empty() {
    let d = start();
    let reply = send(d.addr, "GET", "/api/ports", &authed(), "");

    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert_eq!(reply.json()["ports"].as_array().unwrap().len(), 0);
}

#[test]
fn a_port_can_be_paired_started_and_removed() {
    let d = start();

    let created = send(
        d.addr,
        "POST",
        "/api/ports",
        &authed(),
        r#"{"device":"FAKE0","label":"GPS","protocol":"rfc2217","baud":460800,
            "tcp_port":0,"start":true}"#,
    );
    assert_eq!(created.status, 200, "{}", created.body);

    let port = &created.json()["port"];
    assert_eq!(port["device"], "FAKE0");
    assert_eq!(port["label"], "GPS");
    assert_eq!(port["protocol"], "rfc2217");
    assert_eq!(port["baud"], 460_800);
    assert_eq!(port["data_bits"], 8, "counts serialise as numbers");
    assert_eq!(port["parity"], "none");
    assert_eq!(port["running"], true);
    assert_eq!(port["expose"], false, "loopback unless asked otherwise");
    let id = port["id"].as_str().unwrap().to_owned();
    assert_eq!(id, "fake0");

    let listed = send(d.addr, "GET", "/api/ports", &authed(), "");
    assert_eq!(listed.json()["ports"].as_array().unwrap().len(), 1);

    let stopped = send(
        d.addr,
        "POST",
        &format!("/api/ports/{id}/stop"),
        &authed(),
        "",
    );
    assert_eq!(stopped.status, 200);
    assert_eq!(stopped.json()["port"]["running"], false);

    let removed = send(d.addr, "DELETE", &format!("/api/ports/{id}"), &authed(), "");
    assert_eq!(removed.status, 200);

    let empty = send(d.addr, "GET", "/api/ports", &authed(), "");
    assert_eq!(empty.json()["ports"].as_array().unwrap().len(), 0);
}

#[test]
fn pairing_the_same_device_twice_is_refused() {
    let d = start();
    let body = r#"{"device":"FAKE0","tcp_port":0}"#;

    assert_eq!(
        send(d.addr, "POST", "/api/ports", &authed(), body).status,
        200
    );

    let second = send(d.addr, "POST", "/api/ports", &authed(), body);
    assert_eq!(second.status, 400);
    assert!(
        second.json()["error"]
            .as_str()
            .unwrap()
            .contains("already paired"),
        "got: {}",
        second.body
    );
}

#[test]
fn settings_can_be_changed_and_are_persisted() {
    let d = start();
    send(
        d.addr,
        "POST",
        "/api/ports",
        &authed(),
        r#"{"device":"FAKE0","tcp_port":0,"baud":9600}"#,
    );

    let patched = send(
        d.addr,
        "PATCH",
        "/api/ports/fake0",
        &authed(),
        r#"{"baud":115200,"parity":"even","autostart":true}"#,
    );
    assert_eq!(patched.status, 200, "{}", patched.body);
    assert_eq!(patched.json()["port"]["baud"], 115_200);
    assert_eq!(patched.json()["port"]["parity"], "even");
    assert_eq!(patched.json()["port"]["autostart"], true);

    // What is on disk is what would come back after a restart.
    let saved = Config::load_or_create(d.registry.config_path(), 4001, None).unwrap();
    let port = saved.port("fake0").expect("saved");
    assert_eq!(port.serial.baud, 115_200);
    assert!(port.autostart);
}

#[test]
fn nonsense_line_settings_are_rejected_with_a_reason() {
    let d = start();
    send(
        d.addr,
        "POST",
        "/api/ports",
        &authed(),
        r#"{"device":"FAKE0","tcp_port":0}"#,
    );

    let reply = send(
        d.addr,
        "PATCH",
        "/api/ports/fake0",
        &authed(),
        r#"{"data_bits":9}"#,
    );
    assert_eq!(reply.status, 400);
    assert!(
        reply.json()["error"]
            .as_str()
            .unwrap()
            .contains("data bits"),
        "got: {}",
        reply.body
    );
}

#[test]
fn malformed_json_is_a_bad_request_not_a_crash() {
    let d = start();
    let reply = send(d.addr, "POST", "/api/ports", &authed(), "{not json");
    assert_eq!(reply.status, 400);
    assert!(
        reply.json()["error"]
            .as_str()
            .unwrap()
            .contains("invalid JSON")
    );
}

// -------------------------------------------------------------------- sending

#[test]
fn sending_to_a_stopped_port_explains_itself() {
    let d = start();
    send(
        d.addr,
        "POST",
        "/api/ports",
        &authed(),
        r#"{"device":"FAKE0","tcp_port":0}"#,
    );

    let reply = send(
        d.addr,
        "POST",
        "/api/ports/fake0/send",
        &authed(),
        r#"{"data":"hello"}"#,
    );
    assert_eq!(reply.status, 400);
    assert!(
        reply.json()["error"]
            .as_str()
            .unwrap()
            .contains("not running")
    );
}

#[test]
fn text_and_hex_both_reach_the_device() {
    let d = start();
    send(
        d.addr,
        "POST",
        "/api/ports",
        &authed(),
        r#"{"device":"FAKE0","tcp_port":0,"start":true}"#,
    );

    let text = send(
        d.addr,
        "POST",
        "/api/ports/fake0/send",
        &authed(),
        r#"{"data":"AT","encoding":"text","append":"crlf"}"#,
    );
    assert_eq!(text.status, 200, "{}", text.body);
    assert_eq!(text.json()["sent"], 4, "AT plus CR LF");

    let hex = send(
        d.addr,
        "POST",
        "/api/ports/fake0/send",
        &authed(),
        r#"{"data":"01 02 ff","encoding":"hex"}"#,
    );
    assert_eq!(hex.status, 200, "{}", hex.body);
    assert_eq!(hex.json()["sent"], 3);

    send(d.addr, "POST", "/api/ports/fake0/stop", &authed(), "");
}

#[test]
fn a_bad_encoding_is_named_in_the_error() {
    let d = start();
    send(
        d.addr,
        "POST",
        "/api/ports",
        &authed(),
        r#"{"device":"FAKE0","tcp_port":0,"start":true}"#,
    );

    let reply = send(
        d.addr,
        "POST",
        "/api/ports/fake0/send",
        &authed(),
        r#"{"data":"zz","encoding":"hex"}"#,
    );
    assert_eq!(reply.status, 400);
    assert!(
        reply.json()["error"]
            .as_str()
            .unwrap()
            .contains("hex digit")
    );

    send(d.addr, "POST", "/api/ports/fake0/stop", &authed(), "");
}

// -------------------------------------------------------------------- streams

#[test]
fn the_state_stream_speaks_server_sent_events() {
    let d = start();

    let mut stream = TcpStream::connect(d.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(
            format!(
                "GET /api/events HTTP/1.1\r\nHost: {}\r\nCookie: st={TOKEN}\r\n\r\n",
                d.addr
            )
            .as_bytes(),
        )
        .unwrap();

    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf).expect("headers");
    let head = String::from_utf8_lossy(&buf[..n]).into_owned();

    assert!(head.starts_with("HTTP/1.1 200"), "got: {head}");
    assert!(head.contains("text/event-stream"), "got: {head}");
    assert!(head.contains("no-store"), "got: {head}");

    // The first state frame follows immediately, in the wire format EventSource
    // expects: an event name, a data line, then a blank line.
    let mut seen = head;
    while !seen.contains("\n\n") {
        let n = match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(seen.contains("event: state"), "got: {seen}");
    assert!(seen.contains("data: {"), "got: {seen}");
}

#[test]
fn the_data_stream_is_closed_without_a_token() {
    let d = start();
    let reply = send(d.addr, "GET", "/api/ports/fake0/stream", &[], "");
    assert_eq!(reply.status, 401);
}
