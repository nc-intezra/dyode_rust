//! The local API (cargo feature `api`): each box serves its own status on
//! its own management network, over TLS, to holders of a token.
//!
//! Phase 1 is read-only:
//!
//! | path               | token  | what                                       |
//! |--------------------|--------|--------------------------------------------|
//! | `/v1/health`       | none   | up, side, version, uptime                  |
//! | `/v1/openapi.json` | none   | this API's description (docs/api/)         |
//! | `/v1/status`       | read   | this box, and on the output box the input  |
//! |                    |        | box's last status, with warnings           |
//! | `/metrics`         | read   | the same, in Prometheus text format        |
//! | `/v1/check`        | read   | `dyode check` as JSON                      |
//!
//! Requests are parsed by `httparse`; each connection gets a thread, up to
//! `max_clients`, and one request (`Connection: close`).  Nothing here can
//! reach the other box: the output box only reports what the input box
//! already sent through the diode.

use crate::config::{ApiListen, Config, Side};
use crate::logging::Event;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const MAX_REQUEST: usize = 16 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// The OpenAPI description, built in so an air-gapped box carries its own.
pub const OPENAPI_YAML: &str = include_str!("../docs/api/openapi.yaml");

pub fn openapi_json() -> Value {
    serde_yaml::from_str::<Value>(OPENAPI_YAML).unwrap_or(Value::Null)
}

/// The TLS server configuration from PEM files.
pub fn tls_config(
    cert: &std::path::Path,
    key: &std::path::Path,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("cannot read {}: {e}", cert.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", cert.display()))?;
    if certs.is_empty() {
        return Err(format!("{} holds no certificate", cert.display()));
    }
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{}: {e}", key.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS certificate and key do not work together: {e}"))?;
    Ok(Arc::new(cfg))
}

/// Refuse to listen on the diode NIC, whatever address it was given.
fn check_not_diode(cfg: &Config, side: Side, listen: SocketAddr) -> Result<(), String> {
    let diode_if = match side {
        Side::In => &cfg.network.in_interface,
        Side::Out => &cfg.network.out_interface,
    };
    if let IpAddr::V4(ip) = listen.ip() {
        if crate::netutil::interface_addresses().get(diode_if) == Some(&ip) {
            return Err(format!(
                "api.{}.listen {ip} is on the diode NIC {diode_if}: the API must listen on the management NIC",
                side.as_str()
            ));
        }
    }
    Ok(())
}

struct Ctx {
    cfg: Arc<Config>,
    side: Side,
    api: ApiListen,
    started: Instant,
}

/// Serve for ever (under the supervisor).
pub fn serve(cfg: &Arc<Config>, side: Side, api: &ApiListen) -> Result<(), String> {
    check_not_diode(cfg, side, api.listen)?;
    let tls = match &api.tls {
        Some((c, k)) => Some(tls_config(c, k)?),
        None => None,
    };
    let listener = TcpListener::bind(api.listen)
        .map_err(|e| format!("API: cannot listen on {}: {e}", api.listen))?;
    log::info!(
        "API listening on {} ({}, tokens in {})",
        api.listen,
        if tls.is_some() {
            "TLS"
        } else {
            "plain HTTP, loopback only"
        },
        api.tokens.display()
    );
    let ctx = Arc::new(Ctx {
        cfg: Arc::clone(cfg),
        side,
        api: api.clone(),
        started: Instant::now(),
    });
    serve_on(listener, tls, ctx)
}

fn serve_on(
    listener: TcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    ctx: Arc<Ctx>,
) -> Result<(), String> {
    let active = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        if active.load(Ordering::Relaxed) >= ctx.api.max_clients {
            crate::stats::count("api_busy");
            drop(stream);
            continue;
        }
        active.fetch_add(1, Ordering::Relaxed);
        let (ctx, tls, mine) = (Arc::clone(&ctx), tls.clone(), Arc::clone(&active));
        let spawned = std::thread::Builder::new()
            .name("api-conn".into())
            .spawn(move || {
                let peer = stream.peer_addr().ok();
                let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                let r = match tls {
                    Some(cfg) => match rustls::ServerConnection::new(cfg) {
                        Ok(conn) => handle(&mut rustls::StreamOwned::new(conn, stream), peer, &ctx),
                        Err(e) => Err(e.to_string()),
                    },
                    None => {
                        let mut s = stream;
                        handle(&mut s, peer, &ctx)
                    }
                };
                if let Err(e) = r {
                    log::debug!("API connection from {peer:?}: {e}");
                }
                mine.fetch_sub(1, Ordering::Relaxed);
            });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::Relaxed);
        }
    }
    Err("API listener closed".into())
}

struct Request {
    method: String,
    path: String,
    bearer: Option<String>,
}

fn read_request<S: Read>(s: &mut S) -> Result<Option<Request>, String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let n = s.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(_)) => {
                let bearer = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("authorization"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .and_then(|v| {
                        v.strip_prefix("Bearer ")
                            .or_else(|| v.strip_prefix("bearer "))
                    })
                    .map(|t| t.trim().to_string());
                return Ok(Some(Request {
                    method: req.method.unwrap_or("").to_string(),
                    path: req.path.unwrap_or("").to_string(),
                    bearer,
                }));
            }
            Ok(httparse::Status::Partial) if buf.len() < MAX_REQUEST => continue,
            Ok(httparse::Status::Partial) => return Err("request headers too large".into()),
            Err(e) => return Err(format!("bad request: {e}")),
        }
    }
}

fn respond<S: Write>(
    s: &mut S,
    code: u16,
    ctype: &str,
    body: &[u8],
    head: bool,
    extra: &str,
) -> Result<(), String> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let hdr = format!(
        "HTTP/1.1 {code} {reason}\r\nServer: DYODE\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    s.write_all(hdr.as_bytes()).map_err(|e| e.to_string())?;
    if !head {
        s.write_all(body).map_err(|e| e.to_string())?;
    }
    s.flush().map_err(|e| e.to_string())
}

fn json_body(v: &Value) -> Vec<u8> {
    let mut b = serde_json::to_vec_pretty(v).unwrap_or_default();
    b.push(b'\n');
    b
}

fn error<S: Write>(s: &mut S, code: u16, msg: &str, head: bool, extra: &str) -> Result<(), String> {
    respond(
        s,
        code,
        "application/json",
        &json_body(&json!({"error": msg})),
        head,
        extra,
    )
}

/// Logged to transfer.jsonl, and counted there (events without a module
/// are box-wide counters).
fn refused(ctx: &Ctx, peer: Option<SocketAddr>, path: &str, reason: &str) {
    Event::new("api_refused")
        .field("peer", peer.map(|p| p.ip().to_string()))
        .field("path", path)
        .field("reason", reason)
        .field("side", ctx.side.as_str())
        .emit();
}

fn handle<S: Read + Write>(s: &mut S, peer: Option<SocketAddr>, ctx: &Ctx) -> Result<(), String> {
    let req = match read_request(s) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()),
        Err(e) => return error(s, 400, &e, false, ""),
    };
    let head = req.method == "HEAD";
    if req.method != "GET" && !head {
        return error(s, 405, "only GET and HEAD", false, "Allow: GET, HEAD\r\n");
    }
    let path = req.path.split('?').next().unwrap_or("").to_string();
    let stale = crate::status::stale_after(&ctx.cfg);
    // Open endpoints.
    match path.as_str() {
        "/v1/health" => {
            let v = json!({
                "status": "ok",
                "side": ctx.side.as_str(),
                "version": env!("CARGO_PKG_VERSION"),
                "uptime_s": ctx.started.elapsed().as_secs(),
            });
            return respond(s, 200, "application/json", &json_body(&v), head, "");
        }
        "/v1/openapi.json" => {
            return respond(
                s,
                200,
                "application/json",
                &json_body(&openapi_json()),
                head,
                "",
            );
        }
        "/v1/status" | "/metrics" | "/v1/check" => {}
        _ => return error(s, 404, "no such endpoint (see /v1/openapi.json)", head, ""),
    }
    // Everything else needs a token with the read scope.
    let tokens = match crate::apitoken::list(&ctx.api.tokens) {
        Ok(t) => t,
        Err(e) => {
            log::error!("API: {e}");
            return error(s, 500, "token file unreadable", head, "");
        }
    };
    let Some(presented) = req.bearer else {
        refused(ctx, peer, &path, "no token");
        return error(
            s,
            401,
            "a bearer token is required",
            head,
            "WWW-Authenticate: Bearer realm=\"dyode\"\r\n",
        );
    };
    let Some(token) = crate::apitoken::verify(&tokens, &presented) else {
        refused(ctx, peer, &path, "unknown token");
        return error(
            s,
            401,
            "unknown token",
            head,
            "WWW-Authenticate: Bearer realm=\"dyode\", error=\"invalid_token\"\r\n",
        );
    };
    if !token.allows("read") {
        refused(ctx, peer, &path, "scope");
        return error(s, 403, "this token lacks the read scope", head, "");
    }
    match path.as_str() {
        "/v1/status" => respond(
            s,
            200,
            "application/json",
            &json_body(&crate::stats::combined(stale)),
            head,
            "",
        ),
        "/metrics" => respond(
            s,
            200,
            "text/plain; version=0.0.4; charset=utf-8",
            crate::stats::prometheus(stale).as_bytes(),
            head,
            "",
        ),
        _ => {
            let r = crate::check::report(&ctx.cfg, Some(ctx.side.as_str()), false);
            respond(
                s,
                200,
                "application/json",
                &json_body(&serde_json::to_value(&r).unwrap_or(Value::Null)),
                head,
                "",
            )
        }
    }
}

/// For tests: serve on an already bound listener.
#[doc(hidden)]
pub fn serve_listener(
    listener: TcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    cfg: Arc<Config>,
    side: Side,
    api: ApiListen,
) -> Result<(), String> {
    let ctx = Arc::new(Ctx {
        cfg,
        side,
        api,
        started: Instant::now(),
    });
    serve_on(listener, tls, ctx)
}

/// The routes this server answers, for the test that keeps
/// docs/api/openapi.yaml in step.
pub const ROUTES: [&str; 5] = [
    "/v1/health",
    "/v1/status",
    "/metrics",
    "/v1/check",
    "/v1/openapi.json",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_lists_exactly_the_routes() {
        let v = openapi_json();
        let paths: Vec<String> = v["paths"]
            .as_object()
            .expect("paths")
            .keys()
            .cloned()
            .collect();
        let mut want: Vec<String> = ROUTES.iter().map(|s| s.to_string()).collect();
        let mut got = paths.clone();
        want.sort();
        got.sort();
        assert_eq!(got, want, "docs/api/openapi.yaml and api::ROUTES differ");
        assert_eq!(v["info"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn request_parsing() {
        let mut r: &[u8] =
            b"GET /metrics?x=1 HTTP/1.1\r\nHost: a\r\nAuthorization: Bearer abc \r\n\r\n";
        let q = read_request(&mut r).unwrap().unwrap();
        assert_eq!(
            (q.method.as_str(), q.path.as_str(), q.bearer.as_deref()),
            ("GET", "/metrics?x=1", Some("abc"))
        );
        let mut bad: &[u8] = b"GE\x01T / HTTP/1.1\r\n\r\n";
        assert!(read_request(&mut bad).is_err());
        let big = format!("GET / HTTP/1.1\r\nX: {}\r\n", "a".repeat(MAX_REQUEST));
        let mut b = big.as_bytes();
        assert!(read_request(&mut b).is_err());
    }
}
