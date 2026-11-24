//! Minimal HTTP/1.1 server for the port-2025 API (std only).
//!
//! GET requests only, one request per connection (`Connection: close`).
//! The request line is parsed leniently because clients put raw, unescaped
//! JSON — spaces included — in the query string.
//!
//! [`parse_request`] and [`route`] are pure; [`serve`] does the socket I/O.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::api::{self, ApiInfo};
use crate::bridge::Handle;
use crate::{log_debug, log_info, log_warn};

/// Largest request head accepted.
pub const MAX_REQUEST: usize = 16 * 1024;
/// Concurrent connections served; more are refused with 503.
pub const MAX_CONNECTIONS: usize = 16;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `getSystemData` waits for the bus data before answering 503.
/// Below the 4 s timeout of the common Python client.
pub const READY_WAIT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Raw query string, not decoded.
    pub query: String,
}

/// Parse a request head. `Ok(None)` means more bytes are needed; `Err`
/// carries the HTTP status to answer with.
pub fn parse_request(buf: &[u8]) -> Result<Option<Request>, (u16, String)> {
    let too_large = || Err((413, "request too large".to_string()));
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        if buf.len() > MAX_REQUEST {
            return too_large();
        }
        return Ok(None);
    };
    // Checked on the complete head too, so the limit does not depend on how
    // the bytes were split across reads.
    if end + 4 > MAX_REQUEST {
        return too_large();
    }
    let head = String::from_utf8_lossy(&buf[..end]);
    let line = head.lines().next().unwrap_or("");
    let (method, rest) = line
        .split_once(' ')
        .ok_or((400, "bad request line".to_string()))?;
    // The target runs to the last " HTTP/": raw JSON may contain spaces.
    let target = match rest.rfind(" HTTP/") {
        Some(i) => &rest[..i],
        None => rest,
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Ok(Some(Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
    }))
}

/// Handle one request: returns (status, JSON body). `getSystemData` waits
/// up to `ready_wait` for the bus data (e.g. just after startup).
pub fn route(
    req: &Request,
    handle: &Handle,
    info: &ApiInfo,
    ready_wait: Duration,
) -> (u16, String) {
    if req.method != "GET" {
        return (405, json!({"error": "only GET is supported"}).to_string());
    }
    match req.path.as_str() {
        "/getSystemData" => {
            let deadline = Instant::now() + ready_wait;
            loop {
                match api::system_data(&handle.snapshot(), info, Instant::now()) {
                    Ok(v) => return (200, v.to_string()),
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(100))
                    }
                    Err(e) => return (503, json!({"error": e}).to_string()),
                }
            }
        }
        "/setAircon" => {
            let now = Instant::now();
            let result = handle
                .submit(now, |st| {
                    let writes = api::set_aircon(&req.query, st, now)?;
                    for w in &writes {
                        log_info!("API write: {}", crate::describe(w));
                    }
                    Ok(writes)
                })
                .map_err(|e| e.to_string());
            if let Err(e) = &result {
                log_warn!("setAircon refused: {e}");
            }
            (200, api::ack("setAircon", &result).to_string())
        }
        p @ ("/setLights" | "/setThings") => {
            let name = &p[1..];
            let r = Err(format!("{name} is not supported"));
            (200, api::ack(name, &r).to_string())
        }
        _ => (404, json!({"error": "not found"}).to_string()),
    }
}

pub fn response(status: u16, body: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Service Unavailable",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn handle_conn(mut s: TcpStream, handle: &Handle, info: &ApiInfo, ready_wait: Duration) {
    // One deadline for the whole request head, so a client trickling bytes
    // cannot hold a connection slot indefinitely.
    let deadline = Instant::now() + IO_TIMEOUT;
    let _ = s.set_write_timeout(Some(IO_TIMEOUT));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    let reply = loop {
        match parse_request(&buf) {
            Ok(Some(req)) => {
                log_debug!("http {} {}?{}", req.method, req.path, req.query);
                break route(&req, handle, info, ready_wait);
            }
            Ok(None) => {}
            Err((status, e)) => break (status, json!({"error": e}).to_string()),
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || s.set_read_timeout(Some(left)).is_err() {
            log_debug!("http: request head not received in time");
            return;
        }
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let _ = s.write_all(&response(reply.0, &reply.1));
}

/// Accept connections forever, one thread per connection.
pub fn serve(listener: TcpListener, handle: Handle, info: ApiInfo) {
    serve_with(listener, handle, info, READY_WAIT)
}

/// [`serve`] with a custom readiness wait (tests).
pub fn serve_with(listener: TcpListener, handle: Handle, info: ApiInfo, ready_wait: Duration) {
    let info = Arc::new(info);
    let active = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let mut s = match conn {
            Ok(s) => s,
            Err(e) => {
                log_warn!("http accept: {e}");
                continue;
            }
        };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            let _ = s.write_all(&response(503, r#"{"error":"busy"}"#));
            continue;
        }
        let (handle, info, active) = (handle.clone(), info.clone(), active.clone());
        std::thread::spawn(move || {
            handle_conn(s, &handle, &info, ready_wait);
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Bridge, TxMode};
    use crate::frame::encode;
    use std::net::TcpListener;

    #[test]
    fn parses_raw_unescaped_json_target() {
        let r = parse_request(
            b"GET /setAircon?json={\"aircons\": { \"ac1\": { \"info\" : { \"state\": \"on\" } } } } HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/setAircon");
        assert_eq!(
            r.query,
            "json={\"aircons\": { \"ac1\": { \"info\" : { \"state\": \"on\" } } } }"
        );
        assert_eq!(
            parse_request(b"GET /getSystemData HTTP/1.1\r\n").unwrap(),
            None
        );
        assert_eq!(
            parse_request(&vec![b'a'; MAX_REQUEST + 1]).unwrap_err().0,
            413
        );
        // A complete but oversized head is refused too.
        let mut big = b"GET /x?".to_vec();
        big.resize(MAX_REQUEST, b'a');
        big.extend_from_slice(b" HTTP/1.1\r\n\r\n");
        assert_eq!(parse_request(&big).unwrap_err().0, 413);
        assert_eq!(parse_request(b"nonsense\r\n\r\n").unwrap_err().0, 400);
    }

    fn get(addr: std::net::SocketAddr, target: &str) -> (u16, serde_json::Value) {
        let mut s = TcpStream::connect(addr).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: t\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let (head, body) = out.split_once("\r\n\r\n").unwrap();
        let status = head[9..12].parse().unwrap();
        (status, serde_json::from_str(body).unwrap())
    }

    /// Drive an active bridge to Steady with two zones, then serve on a
    /// loopback port and talk to it over real TCP.
    #[test]
    fn end_to_end_over_tcp() {
        let t = Instant::now();
        let (mut b, h) = Bridge::new(TxMode::Active, t);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let info = ApiInfo::default();
        std::thread::spawn({
            let h = h.clone();
            move || serve_with(listener, h, info, Duration::from_millis(200))
        });

        // Before the bus has delivered anything: 503 so clients retry.
        assert_eq!(get(addr, "/getSystemData").0, 503);

        for f in [
            "Ping",
            "CAN2 in use",
            "Ping",
            "getCAN 1 0703abcde0602030300000000",
            "Ping",
            "Ping",
            "getCAN 1 0703abcde0120010000000000 0703abcde0501010330000100 0703abcde0301e40030000000 0703abcde0401006400000000",
            "Ping",
        ] {
            b.on_bytes(&encode(f), t);
        }
        let (status, d) = get(addr, "/getSystemData");
        assert_eq!(status, 200);
        assert_eq!(d["aircons"]["ac1"]["info"]["state"], "on");
        assert_eq!(d["aircons"]["ac1"]["zones"]["z01"]["value"], 100);

        // Raw unescaped JSON, as phone apps send it.
        let (status, d) = get(
            addr,
            r#"/setAircon?json={"aircons": { "ac1": { "info" : { "state": "off" } } } }"#,
        );
        assert_eq!((status, &d["ack"]), (200, &json!(true)));
        let (_, d) = get(addr, r#"/setAircon?json={"ac1":{"info":{"setTemp":40}}}"#);
        assert_eq!(d["ack"], false);
        assert!(d["reason"].as_str().unwrap().contains("outside"));

        // A second partial update before the box confirms the first keeps
        // the first one's change (off) instead of reverting it.
        let (_, d) = get(addr, r#"/setAircon?json={"ac1":{"info":{"mode":"heat"}}}"#);
        assert_eq!(d["ack"], true);

        // The merged write goes out on the next free slot.
        let out = b.on_bytes(&encode("Ping"), t).unwrap();
        assert_eq!(out, encode("setCAN 0701abcde0500020330000100"));

        assert_eq!(get(addr, "/setLights?json={}").1["ack"], false);
        assert_eq!(get(addr, "/nope").0, 404);
    }

    #[test]
    fn slow_request_head_is_cut_off() {
        let (_b, h) = Bridge::new(TxMode::ListenOnly, Instant::now());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (server, _) = std::thread::scope(|sc| {
            let srv = sc.spawn(move || {
                let (s, _) = listener.accept().unwrap();
                let start = Instant::now();
                handle_conn(s, &h, &ApiInfo::default(), Duration::ZERO);
                start.elapsed()
            });
            let mut c = TcpStream::connect(addr).unwrap();
            // Trickle a byte every second; never finish the head.
            for _ in 0..7 {
                if c.write_all(b"G").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            (srv.join().unwrap(), ())
        });
        assert!(server < IO_TIMEOUT + Duration::from_secs(1), "{server:?}");
    }

    #[test]
    fn get_system_data_waits_for_the_bus() {
        let t = Instant::now();
        let (mut b, h) = Bridge::new(TxMode::ListenOnly, t);
        let req = Request {
            method: "GET".into(),
            path: "/getSystemData".into(),
            query: String::new(),
        };
        let waiter = std::thread::spawn({
            let h = h.clone();
            move || route(&req, &h, &ApiInfo::default(), Duration::from_secs(5)).0
        });
        std::thread::sleep(Duration::from_millis(150));
        b.on_bytes(
            &encode("getCAN 1 0703abcde0120010000000000 0703abcde0501010330000100 0703abcde0301e40030000000 0703abcde0401006400000000"),
            t,
        );
        assert_eq!(waiter.join().unwrap(), 200);
    }

    #[test]
    fn listen_only_refuses_writes() {
        let (mut b, h) = Bridge::new(TxMode::ListenOnly, Instant::now());
        b.on_bytes(
            &encode("getCAN 1 0703abcde0120010000000000 0703abcde0501010330000100 0703abcde0301e40030000000 0703abcde0401006400000000"),
            Instant::now(),
        );
        let req = Request {
            method: "GET".into(),
            path: "/setAircon".into(),
            query: r#"json={"ac1":{"info":{"state":"off"}}}"#.into(),
        };
        let (status, body) = route(&req, &h, &ApiInfo::default(), Duration::ZERO);
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ack"], false);
        assert!(v["reason"].as_str().unwrap().contains("listen-only"));
        // Reads still work.
        let req = Request {
            path: "/getSystemData".into(),
            query: String::new(),
            ..req
        };
        assert_eq!(route(&req, &h, &ApiInfo::default(), Duration::ZERO).0, 200);
        let req = Request {
            method: "POST".into(),
            ..req
        };
        assert_eq!(route(&req, &h, &ApiInfo::default(), Duration::ZERO).0, 405);
    }
}
