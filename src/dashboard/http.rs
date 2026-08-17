//! The web server: routing, the token gate, and the two live streams.
//!
//! Short requests are handled by a small pool of threads. Streams get a thread
//! each, spawned on demand, because a stream occupies its thread for as long as
//! the browser stays open — a couple of monitor panes would otherwise drain the
//! pool and hang the whole dashboard.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde_json::json;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::dashboard::api::{self, ApiError, ApiResult, encode_hex};
use crate::dashboard::registry::Registry;

/// Threads kept ready for ordinary requests.
const POOL_THREADS: usize = 4;
/// Upper bound on live streams, so a stuck tab cannot spawn threads forever.
const MAX_STREAMS: usize = 16;
/// How often a quiet stream emits a comment, to notice a vanished browser.
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// How often the state stream reports.
const STATE_INTERVAL: Duration = Duration::from_secs(1);
/// Nothing this API accepts is large; refuse the rest rather than buffer it.
const MAX_BODY: u64 = 256 * 1024;
/// The cookie holding the access token.
const COOKIE: &str = "st";

pub struct Assets {
    dir: Option<PathBuf>,
}

impl Assets {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self { dir }
    }

    /// The dashboard page. Read from disk when `--assets-dir` was given, so the
    /// UI can be worked on without a recompile; otherwise the copy baked in at
    /// build time, which is what keeps the binary self-contained.
    fn page(&self) -> String {
        match &self.dir {
            Some(dir) => {
                let path = dir.join("app.html");
                std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| format!("<!-- failed to read {}: {e} -->", path.display()))
            }
            None => include_str!("assets/app.html").to_owned(),
        }
    }
}

struct Ctx {
    registry: Arc<Registry>,
    assets: Assets,
    throttle: Throttle,
    streams: AtomicUsize,
}

/// Claim the dashboard's port.
///
/// Separate from [`serve`] so the caller can report the address that was
/// actually bound — and so tests can ask for port 0 and find out what they got.
pub fn bind(addr: &str) -> Result<Server> {
    Server::http(addr)
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("the dashboard could not listen on {addr}"))
}

/// Run the dashboard until the process ends.
pub fn serve(server: Server, registry: Arc<Registry>, assets: Assets) -> Result<()> {
    let server = Arc::new(server);

    let ctx = Arc::new(Ctx {
        registry,
        assets,
        throttle: Throttle::default(),
        streams: AtomicUsize::new(0),
    });

    let mut workers = Vec::new();
    for n in 0..POOL_THREADS {
        let server = Arc::clone(&server);
        let ctx = Arc::clone(&ctx);
        workers.push(
            thread::Builder::new()
                .name(format!("http-{n}"))
                .spawn(move || {
                    while let Ok(request) = server.recv() {
                        handle(&ctx, request);
                    }
                })
                .context("failed to spawn an HTTP worker")?,
        );
    }

    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

fn handle(ctx: &Arc<Ctx>, mut request: Request) {
    let url = request.url().to_owned();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (url.clone(), String::new()),
    };
    let method = request.method().clone();
    let peer = request.remote_addr().map(|a| a.ip());

    // Where a request came from is settled before anything else, including the
    // token: an address that is not allowed should not get as far as learning
    // whether its guess was right.
    if let Some(ip) = peer
        && !ctx.registry.allowlist().permits(ip)
    {
        log::warn!(
            "refused a dashboard request from {ip}, which is outside {}",
            ctx.registry.allowlist().describe()
        );
        return respond_error(
            request,
            403,
            "this address is not allowed to reach the dashboard",
        );
    }

    let require_token = ctx.registry.require_token();

    // The one route reachable without the cookie: arriving with the token in the
    // URL is how you get the cookie in the first place. EventSource cannot set
    // headers, so a cookie is the only thing the live streams can carry.
    if require_token
        && path == "/"
        && method == Method::Get
        && let Some(token) = query_param(&query, "token")
    {
        if constant_time_eq(token.as_bytes(), ctx.registry.token().as_bytes()) {
            ctx.throttle.forget(peer);
            return redirect_home(request, &token);
        }
        return reject(ctx, request, peer);
    }

    if require_token && !authorized(&request, ctx.registry.token()) {
        return reject(ctx, request, peer);
    }

    // Cookies ride along on cross-site requests to this origin, so a mutating
    // route needs more than one. SameSite=Strict covers most browsers; the
    // header below cannot be set cross-origin without a preflight we never
    // grant, and a foreign Origin is refused outright.
    let mutating = matches!(
        method,
        Method::Post | Method::Patch | Method::Put | Method::Delete
    );
    if mutating && let Err(message) = check_same_origin(&request) {
        return respond_error(request, 403, &message);
    }

    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

    match (&method, segments.as_slice()) {
        (Method::Get, ["api", "events"]) => stream_state(ctx, request),
        (Method::Get, ["api", "ports", id, "stream"]) => {
            let id = (*id).to_owned();
            stream_data(ctx, request, id)
        }

        (Method::Get, ["api", "devices"]) => finish(request, api::devices(&ctx.registry)),
        (Method::Get, ["api", "ports"]) => finish(request, api::ports(&ctx.registry)),
        (Method::Post, ["api", "ports"]) => {
            let body = read_body(&mut request);
            let result = body.and_then(|b| api::create(&ctx.registry, &b));
            finish(request, result)
        }
        (Method::Patch, ["api", "ports", id]) => {
            let id = (*id).to_owned();
            let body = read_body(&mut request);
            let result = body.and_then(|b| api::update(&ctx.registry, &id, &b));
            finish(request, result)
        }
        (Method::Delete, ["api", "ports", id]) => finish(request, api::delete(&ctx.registry, id)),
        (Method::Post, ["api", "ports", id, "start"]) => {
            finish(request, api::start(&ctx.registry, id))
        }
        (Method::Post, ["api", "ports", id, "stop"]) => {
            finish(request, api::stop(&ctx.registry, id))
        }
        (Method::Post, ["api", "ports", id, "send"]) => {
            let id = (*id).to_owned();
            let body = read_body(&mut request);
            let result = body.and_then(|b| api::send(&ctx.registry, &id, &b));
            finish(request, result)
        }

        (Method::Get, [""]) | (Method::Get, ["index.html"]) => {
            let page = ctx.assets.page();
            let response = Response::from_string(page)
                .with_header(header("Content-Type", "text/html; charset=utf-8"))
                // The page embeds the whole UI, so never let a stale copy stick
                // around after an upgrade.
                .with_header(header("Cache-Control", "no-store"));
            let _ = request.respond(response);
        }

        _ => respond_error(request, 404, "no such endpoint"),
    }
}

// ------------------------------------------------------------------ streams

/// Live traffic for one port.
fn stream_data(ctx: &Arc<Ctx>, request: Request, id: String) {
    let Some(entry) = ctx.registry.entry(&id) else {
        respond_error(request, 404, &format!("no port with id {id}"));
        return;
    };

    let Some(guard) = StreamGuard::acquire(ctx) else {
        respond_error(request, 503, "too many live streams already open");
        return;
    };

    let name = thread::Builder::new().name(format!("stream-{id}"));
    let spawned = name.spawn(move || {
        let _guard = guard;
        let subscription = entry.tap.subscribe();
        let mut out = request.into_writer();
        if sse_headers(&mut out).is_err() {
            return;
        }

        loop {
            let wrote = match subscription.recv_timeout(PING_INTERVAL) {
                Ok(frame) => {
                    let payload = json!({
                        "dir": frame.dir.as_str(),
                        "t": frame.at_ms,
                        "hex": encode_hex(&frame.data),
                        "dropped": subscription.dropped(),
                    });
                    sse_event(&mut out, "data", &payload.to_string())
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => sse_comment(&mut out),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            // A failed write is how a closed browser tab reaches us.
            if wrote.is_err() {
                break;
            }
        }
    });

    if let Err(e) = spawned {
        log::warn!("could not spawn a stream thread: {e}");
    }
}

/// Port state for everything, once a second. Cheap enough for every open tab.
fn stream_state(ctx: &Arc<Ctx>, request: Request) {
    let Some(guard) = StreamGuard::acquire(ctx) else {
        respond_error(request, 503, "too many live streams already open");
        return;
    };

    let ctx = Arc::clone(ctx);
    let spawned = thread::Builder::new()
        .name("stream-state".to_owned())
        .spawn(move || {
            let _guard = guard;
            let mut out = request.into_writer();
            if sse_headers(&mut out).is_err() {
                return;
            }

            loop {
                let payload = match api::ports(&ctx.registry) {
                    Ok(value) => value.to_string(),
                    Err(e) => json!({ "error": e.message }).to_string(),
                };
                if sse_event(&mut out, "state", &payload).is_err() {
                    break;
                }
                thread::sleep(STATE_INTERVAL);
            }
        });

    if let Err(e) = spawned {
        log::warn!("could not spawn the state stream thread: {e}");
    }
}

/// Keeps the live-stream count honest even if a stream thread panics.
struct StreamGuard(Arc<Ctx>);

impl StreamGuard {
    fn acquire(ctx: &Arc<Ctx>) -> Option<Self> {
        let taken = ctx.streams.fetch_add(1, Ordering::Relaxed);
        if taken >= MAX_STREAMS {
            ctx.streams.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some(Self(Arc::clone(ctx)))
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.streams.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Written by hand rather than through a `Response`, so each event can be
/// flushed the moment it happens instead of sitting in a buffer.
fn sse_headers(out: &mut dyn Write) -> std::io::Result<()> {
    out.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: text/event-stream\r\n\
          Cache-Control: no-store\r\n\
          Connection: close\r\n\
          X-Accel-Buffering: no\r\n\
          \r\n",
    )?;
    out.flush()
}

fn sse_event(out: &mut dyn Write, event: &str, data: &str) -> std::io::Result<()> {
    write!(out, "event: {event}\ndata: {data}\n\n")?;
    out.flush()
}

fn sse_comment(out: &mut dyn Write) -> std::io::Result<()> {
    out.write_all(b": ping\n\n")?;
    out.flush()
}

// --------------------------------------------------------------------- auth

fn authorized(request: &Request, token: &str) -> bool {
    cookie(request, COOKIE)
        .is_some_and(|value| constant_time_eq(value.as_bytes(), token.as_bytes()))
}

/// Comparison that does not finish early on the first wrong byte. The length is
/// allowed to leak: tokens are a fixed, published size.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn check_same_origin(request: &Request) -> Result<(), String> {
    if header_value(request, "X-Requested-With").as_deref() != Some("serial-tcp") {
        return Err("missing X-Requested-With header".to_owned());
    }

    if let Some(origin) = header_value(request, "Origin") {
        let host = header_value(request, "Host").unwrap_or_default();
        let origin_host = origin
            .split_once("://")
            .map_or(origin.as_str(), |(_, rest)| rest);
        if origin_host != host {
            return Err(format!("request from another origin ({origin})"));
        }
    }

    Ok(())
}

fn reject(ctx: &Arc<Ctx>, request: Request, peer: Option<IpAddr>) {
    // Slow repeated guesses down. A token is 32 random bytes, so this is belt
    // and braces, but it also stops a broken client hammering the dashboard.
    let delay = ctx.throttle.note_failure(peer);
    if !delay.is_zero() {
        thread::sleep(delay);
    }

    let body = "<!doctype html><meta charset=utf-8>\
                <title>serial-tcp</title>\
                <p style=\"font:16px system-ui;padding:2rem\">\
                Access token required. Open this page as \
                <code>/?token=YOUR_TOKEN</code> — the token is printed in the \
                console where the dashboard was started.</p>";

    let response = Response::from_string(body)
        .with_status_code(StatusCode(401))
        .with_header(header("Content-Type", "text/html; charset=utf-8"));
    let _ = request.respond(response);
}

fn redirect_home(request: Request, token: &str) {
    // HttpOnly keeps the token out of reach of any script on the page; Strict
    // means it is not attached to requests started from other sites.
    let cookie = format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000");
    let response = Response::empty(StatusCode(302))
        .with_header(header("Location", "/"))
        .with_header(header("Set-Cookie", &cookie));
    let _ = request.respond(response);
}

#[derive(Default)]
struct Throttle {
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl Throttle {
    fn note_failure(&self, peer: Option<IpAddr>) -> Duration {
        let Some(ip) = peer else {
            return Duration::from_millis(100);
        };

        let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let entry = failures.entry(ip).or_insert((0, Instant::now()));
        if entry.1.elapsed() > Duration::from_secs(300) {
            *entry = (0, Instant::now());
        }
        entry.0 = entry.0.saturating_add(1);
        entry.1 = Instant::now();

        let step = entry.0.min(6);
        Duration::from_millis((100u64 << step).min(5_000))
    }

    fn forget(&self, peer: Option<IpAddr>) {
        if let Some(ip) = peer {
            self.failures
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&ip);
        }
    }
}

// ------------------------------------------------------------------ helpers

fn finish(request: Request, result: ApiResult) {
    match result {
        Ok(value) => {
            let response = Response::from_string(value.to_string())
                .with_header(header("Content-Type", "application/json"))
                .with_header(header("Cache-Control", "no-store"));
            let _ = request.respond(response);
        }
        Err(ApiError { status, message }) => respond_error(request, status, &message),
    }
}

fn respond_error(request: Request, status: u16, message: &str) {
    let body = json!({ "error": message }).to_string();
    let response = Response::from_string(body)
        .with_status_code(StatusCode(status))
        .with_header(header("Content-Type", "application/json"));
    let _ = request.respond(response);
}

fn read_body(request: &mut Request) -> Result<String, ApiError> {
    let mut body = String::new();
    request
        .as_reader()
        .take(MAX_BODY)
        .read_to_string(&mut body)
        .map_err(|e| ApiError::bad_request(format!("could not read the request body: {e}")))?;
    Ok(body)
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .unwrap_or_else(|()| panic!("{name} is not a valid header name"))
}

fn header_value(request: &Request, name: &str) -> Option<String> {
    // `HeaderField::equiv` only accepts a `&'static str`, which rules out
    // passing a borrowed name through; comparing the text directly is equivalent
    // and header names are ASCII-insensitive by definition.
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_owned())
}

fn cookie(request: &Request, name: &str) -> Option<String> {
    let header = header_value(request, "Cookie")?;
    header.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then(|| value.trim().to_owned())
    })
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_still_compares_correctly() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn query_params_are_decoded() {
        assert_eq!(query_param("token=abc", "token").as_deref(), Some("abc"));
        assert_eq!(query_param("a=1&token=xy", "token").as_deref(), Some("xy"));
        assert_eq!(query_param("a=1", "token"), None);
        assert_eq!(percent_decode("a%2Fb+c"), "a/b c");
    }
}
