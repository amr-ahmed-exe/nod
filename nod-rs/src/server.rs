//! Embedded local HTTP + SSE server with owner-scoped desktop token authentication.

use crate::api::{self, Ctx, Reply};
use crate::db::Store;
use crate::runtime::storage;
use crate::runtime::RuntimeManager;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use tiny_http::{Header, Method, Response, Server, StatusCode};

include!(concat!(env!("OUT_DIR"), "/embedded_web.rs"));

const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const AUTH_COOKIE_NAME: &str = "nod_desktop";
const AUTH_HEADER_NAME: &str = "x-nod-desktop-token";

/// Running local HTTP/SSE desktop backend bound strictly to loopback `127.0.0.1`.
pub struct LocalServer {
    pub origin: String,
    #[allow(dead_code)]
    pub port: u16,
    pub auth_token: String,
    pub owner_id: String,
    bootstrap_nonce: String,
    pub store: Arc<Store>,
    pub live: Arc<Live>,
    pub runtime: Arc<RuntimeManager>,
}

/// Shared SSE broadcaster used by both API mutations and `RuntimeManager` transitions.
pub struct Live {
    subscribers: Mutex<Vec<(u64, Sender<String>)>>,
    next_id: AtomicU64,
    origin: OnceLock<String>,
}

impl Default for Live {
    fn default() -> Self {
        Self::new()
    }
}

impl Live {
    /// Creates an empty SSE subscriber registry.
    pub fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            origin: OnceLock::new(),
        }
    }

    pub fn set_origin(&self, value: String) {
        let _ = self.origin.set(value);
    }

    pub fn origin(&self) -> &str {
        self.origin
            .get()
            .map(String::as_str)
            .unwrap_or("http://127.0.0.1:3000")
    }

    /// Broadcasts a JSON event frame to all active `/api/events` listeners.
    pub fn broadcast(&self, value: &Value) {
        let payload = format!("data: {value}\n\n");
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|(_, sender)| sender.send(payload.clone()).is_ok());
        }
    }

    /// Emits a topic refresh event on `/api/events`.
    pub fn notify(&self, topic: &str) {
        self.broadcast(&json!({ "type": "refresh", "topic": topic }));
    }
}

struct ServerContext {
    port: u16,
    allow_remote_host: bool,
    auth_token: String,
    owner_id: String,
    bootstrap_nonce: String,
    bootstrap_used: AtomicBool,
    store: Arc<Store>,
    live: Arc<Live>,
    runtime: Arc<RuntimeManager>,
    web_root: Option<PathBuf>,
}

impl LocalServer {
    pub fn start_bound(host: &str, port: u16) -> Result<Self, Box<dyn std::error::Error>> {
        let store = Arc::new(
            Store::open().map_err(|error| format!("Could not open local storage: {error}"))?,
        );
        Self::start_with_store_bound(store, host, port)
    }

    #[cfg(test)]
    pub fn start_with_store(store: Arc<Store>) -> Result<Self, Box<dyn std::error::Error>> {
        Self::start_with_store_bound(store, "127.0.0.1", 0)
    }

    pub fn start_with_store_bound(
        store: Arc<Store>,
        bind_host: &str,
        bind_port: u16,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let web_root = find_web_root();
        let addr = format!("{bind_host}:{bind_port}");
        let server = Server::http(&addr)
            .map_err(|error| format!("Failed to bind local server: {error}"))?;
        let port = server
            .server_addr()
            .to_ip()
            .ok_or("No bound server address")?
            .port();
        let origin_host = if bind_host == "0.0.0.0" {
            "127.0.0.1"
        } else {
            bind_host
        };
        let origin = format!("http://{origin_host}:{port}");
        let allow_remote_host = bind_host != "127.0.0.1" && bind_host != "localhost";

        api::seed(store.as_ref());
        let live = Arc::new(Live::new());
        live.set_origin(origin.clone());
        api::channels::ensure_background_listeners(Arc::clone(&store), Arc::clone(&live));
        let runtime = RuntimeManager::new(Arc::clone(&live));
        let auth_token = storage::random_hex(32);
        let bootstrap_nonce = storage::random_hex(32);
        let owner_id = api::OWNER.to_string();

        let shared = Arc::new(ServerContext {
            port,
            allow_remote_host,
            auth_token: auth_token.clone(),
            owner_id: owner_id.clone(),
            bootstrap_nonce: bootstrap_nonce.clone(),
            bootstrap_used: AtomicBool::new(false),
            store: Arc::clone(&store),
            live: Arc::clone(&live),
            runtime: Arc::clone(&runtime),
            web_root,
        });

        thread::spawn(move || {
            for request in server.incoming_requests() {
                let ctx = Arc::clone(&shared);
                let path = request
                    .url()
                    .split('?')
                    .next()
                    .unwrap_or("/")
                    .to_string();
                if path == "/api/events" {
                    thread::spawn(move || handle_live_events(request, &ctx));
                } else {
                    thread::spawn(move || handle_request(request, &ctx));
                }
            }
        });

        Ok(Self {
            origin,
            port,
            auth_token,
            owner_id,
            bootstrap_nonce,
            store,
            live,
            runtime,
        })
    }

    /// Builds the single-use bootstrap URL that seeds the WebView `HttpOnly; SameSite=Strict` cookie.
    pub fn bootstrap_url(&self, next_path: &str) -> String {
        let safe_next = if next_path == "/setup.html" {
            "/setup.html"
        } else {
            "/"
        };
        format!(
            "{}/__nod_boot?t={}&next={}",
            self.origin, self.bootstrap_nonce, safe_next
        )
    }
}

fn handle_request(mut request: tiny_http::Request, ctx: &ServerContext) {
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("/").to_string();
    let mut query = url
        .split_once('?')
        .map(|(_, q)| parse_query(q))
        .unwrap_or_default();
    query.remove("__x_nod_bridge");
    query.remove("__req_origin");
    if let Some(bridge_hdr) = request_header(&request, "X-Nod-Bridge") {
        query.insert("__x_nod_bridge".to_string(), bridge_hdr.to_string());
    }

    let host_ok = ctx.allow_remote_host || host_allowed(request_header(&request, "Host"), ctx.port);
    let req_origin = request_header(&request, "Origin").map(str::to_string);
    if let Some(origin_hdr) = req_origin.as_deref() {
        query.insert("__req_origin".to_string(), origin_hdr.to_string());
    }
    let origin_ok = ctx.allow_remote_host || origin_allowed(req_origin.as_deref(), ctx.port);

    if !host_ok || !origin_ok {
        send_json_error(
            request,
            403,
            "Request origin is not permitted.",
            None,
        );
        return;
    }

    if path == "/__nod_boot" {
        handle_bootstrap(request, &query, ctx);
        return;
    }

    let cors_origin = req_origin
        .as_deref()
        .filter(|value| origin_allowed(Some(value), ctx.port));

    if request.method() == &Method::Options {
        let mut response = Response::empty(StatusCode(204));
        add_security_headers(&mut response, cors_origin);
        let _ = request.respond(response);
        return;
    }

    if !path.starts_with("/api/") {
        serve_static(request, &path, ctx.web_root.as_deref(), cors_origin);
        return;
    }

    let oauth_callback = request.method() == &Method::Get
        && matches!(path.as_str(), "/api/apps/callback" | "/api/models/oauth/callback")
        && query
            .get("state")
            .map(|s| {
                !s.is_empty()
                    && s.len() <= 128
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            })
            .unwrap_or(false);

    let app_connect_bridge = matches!(request.method(), &Method::Get | &Method::Post)
        && path
            .strip_prefix("/api/apps/connect/")
            .map(|cap| {
                cap.len() == 43
                    && cap
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            .unwrap_or(false);

    if !oauth_callback && !app_connect_bridge && !is_request_authorized(&request, &ctx.auth_token) {
        send_json_error(
            request,
            401,
            "Desktop authentication required.",
            cors_origin,
        );
        return;
    }

    let mut body_bytes = Vec::new();
    let mut limited = request.as_reader().take((MAX_BODY_BYTES + 1) as u64);
    if limited.read_to_end(&mut body_bytes).is_err() {
        send_json_error(request, 400, "Could not read request body.", cors_origin);
        return;
    }
    if body_bytes.len() > MAX_BODY_BYTES {
        send_json_error(request, 413, "Request payload exceeds limit.", cors_origin);
        return;
    }

    let content_type = request_header(&request, "Content-Type")
        .unwrap_or_default()
        .to_string();
    let body = if body_bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body_bytes).unwrap_or(Value::Null)
    };
    if contains_nul(&body)
        || path.contains('\0')
        || query
            .iter()
            .any(|(k, v)| k.contains('\0') || v.contains('\0'))
    {
        send_json_error(
            request,
            400,
            "Text cannot contain a null character (U+0000). Remove it and try again.",
            cors_origin,
        );
        return;
    }
    let method = request.method().as_str().to_string();
    let api_ctx = Ctx {
        method: &method,
        path: &path,
        query: &query,
        body,
        body_bytes: &body_bytes,
        content_type: &content_type,
        store: &ctx.store,
        live: &ctx.live,
        runtime: &ctx.runtime,
        owner: &ctx.owner_id,
    };
    send_reply(request, api::dispatch(&api_ctx), cors_origin);
}

fn handle_bootstrap(
    request: tiny_http::Request,
    query: &HashMap<String, String>,
    ctx: &ServerContext,
) {
    let token_matches = query
        .get("t")
        .map(|v| constant_time_eq(v, &ctx.bootstrap_nonce))
        .unwrap_or(false);
    if !token_matches {
        send_json_error(request, 403, "Invalid desktop bootstrap token.", None);
        return;
    }
    if ctx
        .bootstrap_used
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        send_json_error(request, 403, "Desktop bootstrap token already used.", None);
        return;
    }
    let next = match query.get("next").map(String::as_str) {
        Some("/setup.html") => "/setup.html",
        _ => "/",
    };
    let cookie = format!(
        "{AUTH_COOKIE_NAME}={}; HttpOnly; SameSite=Strict; Path=/",
        ctx.auth_token
    );
    let mut headers = Vec::new();
    push_header(&mut headers, "Location", next);
    push_header(&mut headers, "Set-Cookie", &cookie);
    push_header(&mut headers, "Cache-Control", "no-store");
    let mut response = Response::empty(StatusCode(302));
    for hdr in headers {
        response.add_header(hdr);
    }
    add_security_headers(&mut response, None);
    let _ = request.respond(response);
}

fn handle_live_events(request: tiny_http::Request, ctx: &ServerContext) {
    let host_ok = ctx.allow_remote_host || host_allowed(request_header(&request, "Host"), ctx.port);
    let req_origin = request_header(&request, "Origin").map(str::to_string);
    let origin_ok = ctx.allow_remote_host || origin_allowed(req_origin.as_deref(), ctx.port);
    let cors_origin = req_origin
        .as_deref()
        .filter(|value| origin_allowed(Some(value), ctx.port));

    if !host_ok || !origin_ok {
        send_json_error(request, 403, "Event stream origin not allowed.", None);
        return;
    }
    if !is_request_authorized(&request, &ctx.auth_token) {
        send_json_error(request, 401, "Desktop authentication required.", cors_origin);
        return;
    }

    let id = ctx.live.next_id.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = mpsc::channel();
    if let Ok(mut subscribers) = ctx.live.subscribers.lock() {
        subscribers.push((id, sender));
    }
    let mut headers = Vec::new();
    push_header(&mut headers, "Content-Type", "text/event-stream; charset=utf-8");
    push_header(&mut headers, "Cache-Control", "no-cache, no-store");
    push_header(&mut headers, "Connection", "keep-alive");
    push_header(&mut headers, "X-Accel-Buffering", "no");

    let mut response = Response::new(
        StatusCode(200),
        headers,
        EventStream {
            receiver,
            buffer: Vec::new(),
            offset: 0,
            started: false,
        },
        None,
        None,
    );
    add_security_headers(&mut response, cors_origin);
    let _ = request.respond(response);
    if let Ok(mut subscribers) = ctx.live.subscribers.lock() {
        subscribers.retain(|(subscriber, _)| *subscriber != id);
    }
}

fn send_reply(request: tiny_http::Request, reply: Reply, cors_origin: Option<&str>) {
    let (bytes, content_type) = reply.raw.unwrap_or_else(|| {
        (
            reply.body.to_string().into_bytes(),
            "application/json; charset=utf-8".to_string(),
        )
    });
    let length = bytes.len();
    let mut headers = Vec::new();
    if let Some(location) = reply.location.as_deref() {
        push_header(&mut headers, "Location", location);
    }
    push_header(&mut headers, "Content-Type", &content_type);
    push_header(&mut headers, "Cache-Control", "no-store");
    for (name, val) in &reply.extra_headers {
        push_header(&mut headers, name, val);
    }
    let mut response = Response::new(
        StatusCode(reply.status),
        headers,
        std::io::Cursor::new(bytes),
        Some(length),
        None,
    );
    add_security_headers(&mut response, cors_origin);
    let _ = request.respond(response);
}

fn send_json_error(
    request: tiny_http::Request,
    status: u16,
    message: &str,
    cors_origin: Option<&str>,
) {
    send_reply(request, Reply::error(status, message), cors_origin);
}

fn request_header<'a>(request: &'a tiny_http::Request, name: &str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}

fn is_request_authorized(request: &tiny_http::Request, expected_token: &str) -> bool {
    if let Some(value) = request_header(request, AUTH_HEADER_NAME)
        && constant_time_eq(value.trim(), expected_token) {
            return true;
        }
    if let Some(auth) = request_header(request, "Authorization")
        && let Some(bearer) = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer "))
            && constant_time_eq(bearer.trim(), expected_token) {
                return true;
            }
    if let Some(cookie_header) = request_header(request, "Cookie") {
        for part in cookie_header.split(';') {
            if let Some((name, val)) = part.trim().split_once('=')
                && name.trim() == AUTH_COOKIE_NAME && constant_time_eq(val.trim(), expected_token) {
                    return true;
                }
        }
    }
    false
}

fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else {
        return true;
    };
    let lower = host.trim().to_ascii_lowercase();
    lower == format!("127.0.0.1:{port}") || lower == format!("localhost:{port}")
}

fn origin_allowed(origin: Option<&str>, port: u16) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let lower = origin.trim().trim_end_matches('/').to_ascii_lowercase();
    lower == format!("http://127.0.0.1:{port}") || lower == format!("http://localhost:{port}")
}

fn contains_nul(value: &Value) -> bool {
    let mut stack = vec![value];
    while let Some(item) = stack.pop() {
        match item {
            Value::String(s) if s.contains('\0') => return true,
            Value::Array(items) => stack.extend(items.iter()),
            Value::Object(map) => {
                for (k, v) in map {
                    if k.contains('\0') {
                        return true;
                    }
                    stack.push(v);
                }
            }
            _ => {}
        }
    }
    false
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let lb = left.as_bytes();
    let rb = right.as_bytes();
    if lb.len() != rb.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in lb.iter().zip(rb.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            if key.is_empty() {
                None
            } else {
                Some((key.to_string(), value.to_string()))
            }
        })
        .collect()
}

fn is_valid_web_root(path: &Path) -> bool {
    path.join("index.html").is_file() && path.join("setup.html").is_file()
}

fn find_web_root() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("NOD_WEB_ROOT") {
        let path = PathBuf::from(custom);
        if is_valid_web_root(&path) {
            return Some(path);
        }
    }

    let candidates = [
        PathBuf::from("web"),
        PathBuf::from("../web"),
        PathBuf::from("../../web"),
        PathBuf::from("../src-ts/dist/web"),
        PathBuf::from("src-ts/dist/web"),
        PathBuf::from("../../src-ts/dist/web"),
        PathBuf::from("../../../src-ts/dist/web"),
    ];

    if let Ok(exe) = std::env::current_exe()
        && let Some(directory) = exe.parent() {
            for candidate in &candidates {
                let path = directory.join(candidate);
                if is_valid_web_root(&path) {
                    return Some(path.canonicalize().unwrap_or(path));
                }
            }
        }

    if let Ok(directory) = std::env::current_dir() {
        for candidate in &candidates {
            let path = directory.join(candidate);
            if is_valid_web_root(&path) {
                return Some(path.canonicalize().unwrap_or(path));
            }
        }
    }

    None
}

fn serve_static(
    request: tiny_http::Request,
    path: &str,
    web_root: Option<&Path>,
    cors_origin: Option<&str>,
) {
    let relative = path.trim_start_matches('/');
    if relative.contains("..") || relative.contains('\\') {
        send_json_error(request, 404, "Not Found", cors_origin);
        return;
    }
    let candidate = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };

    if let Some(root) = web_root {
        let file = root.join(candidate);
        if file.is_file() {
            serve_file(request, &file, candidate, cors_origin);
            return;
        }
        let index = root.join("index.html");
        if index.is_file() {
            serve_file(request, &index, "index.html", cors_origin);
            return;
        }
    }

    if let Some(bytes) = embedded(candidate) {
        serve_bytes(request, candidate, bytes, cors_origin);
        return;
    }
    if let Some(bytes) = embedded("index.html") {
        serve_bytes(request, "index.html", bytes, cors_origin);
        return;
    }

    let mut response = Response::from_string("Not Found").with_status_code(StatusCode(404));
    add_security_headers(&mut response, cors_origin);
    let _ = request.respond(response);
}

fn serve_file(
    request: tiny_http::Request,
    path: &Path,
    name: &str,
    cors_origin: Option<&str>,
) {
    match fs::read(path) {
        Ok(data) => serve_bytes(request, name, &data, cors_origin),
        Err(_) => {
            let mut response =
                Response::from_string("Error reading file").with_status_code(StatusCode(500));
            add_security_headers(&mut response, cors_origin);
            let _ = request.respond(response);
        }
    }
}

fn serve_bytes(
    request: tiny_http::Request,
    name: &str,
    data: &[u8],
    cors_origin: Option<&str>,
) {
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let mut headers = Vec::new();
    push_header(&mut headers, "Content-Type", mime.as_ref());
    if name.starts_with("assets/") {
        push_header(&mut headers, "Cache-Control", "public, max-age=31536000, immutable");
    } else {
        push_header(&mut headers, "Cache-Control", "no-cache");
    }
    let mut response = Response::new(StatusCode(200), headers, data, Some(data.len()), None);
    add_security_headers(&mut response, cors_origin);
    let _ = request.respond(response);
}

struct EventStream {
    receiver: Receiver<String>,
    buffer: Vec<u8>,
    offset: usize,
    started: bool,
}

impl Read for EventStream {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.offset >= self.buffer.len() {
            self.buffer.clear();
            self.offset = 0;
            if !self.started {
                self.started = true;
                self.buffer = sse_frame(": connected\n\n");
            } else {
                match self.receiver.recv_timeout(Duration::from_secs(15)) {
                    Ok(message) => self.buffer = sse_frame(&message),
                    Err(RecvTimeoutError::Timeout) => {
                        self.buffer = sse_frame(": keepalive\n\n");
                    }
                    Err(RecvTimeoutError::Disconnected) => return Ok(0),
                }
            }
        }
        let count = (self.buffer.len() - self.offset).min(output.len());
        output[..count].copy_from_slice(&self.buffer[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

fn sse_frame(payload: &str) -> Vec<u8> {
    // tiny_http streams an unknown-length body through chunked_transfer::Encoder,
    // which only pushes a chunk downstream when a single write overflows its
    // 8 KiB buffer. Keeping every frame at least 8 KiB + 1 forces immediate flush.
    const FRAME: usize = 8 * 1024 + 1;
    let mut frame = Vec::with_capacity(FRAME.max(payload.len()));
    frame.extend_from_slice(payload.as_bytes());
    if frame.len() < FRAME {
        frame.resize(FRAME, b'\n');
    }
    frame
}

fn push_header(headers: &mut Vec<Header>, name: &str, value: &str) {
    if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
        headers.push(header);
    }
}

fn add_security_headers<R: Read>(response: &mut Response<R>, cors_origin: Option<&str>) {
    if let Ok(hdr) = Header::from_bytes("X-Content-Type-Options", "nosniff") {
        response.add_header(hdr);
    }
    if let Ok(hdr) = Header::from_bytes("X-Frame-Options", "DENY") {
        response.add_header(hdr);
    }
    if let Some(origin) = cors_origin {
        if let Ok(hdr) = Header::from_bytes("Access-Control-Allow-Origin", origin.as_bytes()) {
            response.add_header(hdr);
        }
        if let Ok(hdr) = Header::from_bytes("Access-Control-Allow-Credentials", "true") {
            response.add_header(hdr);
        }
        if let Ok(hdr) = Header::from_bytes("Vary", "Origin") {
            response.add_header(hdr);
        }
        if let Ok(hdr) = Header::from_bytes(
            "Access-Control-Allow-Methods",
            "GET, POST, PUT, PATCH, DELETE, OPTIONS",
        ) {
            response.add_header(hdr);
        }
        if let Ok(hdr) = Header::from_bytes(
            "Access-Control-Allow-Headers",
            "Content-Type, Authorization, X-Nod-Desktop-Token",
        ) {
            response.add_header(hdr);
        }
    }
}

fn embedded(path: &str) -> Option<&'static [u8]> {
    static FILES: OnceLock<HashMap<&'static str, &'static [u8]>> = OnceLock::new();
    FILES
        .get_or_init(|| EMBEDDED_WEB.iter().copied().collect())
        .get(path)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_origin_reject_non_loopback() {
        assert!(host_allowed(Some("127.0.0.1:4567"), 4567));
        assert!(host_allowed(Some("localhost:4567"), 4567));
        assert!(!host_allowed(Some("evil.example:4567"), 4567));
        assert!(!host_allowed(Some("127.0.0.1:9999"), 4567));

        assert!(origin_allowed(Some("http://127.0.0.1:4567"), 4567));
        assert!(origin_allowed(Some("http://localhost:4567/"), 4567));
        assert!(!origin_allowed(Some("https://evil.example"), 4567));
        assert!(!origin_allowed(Some("http://127.0.0.1:1234"), 4567));
    }

    #[test]
    fn api_requires_valid_desktop_token_or_cookie() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let server = LocalServer::start_with_store(store).unwrap();
        let port = server.port;
        let agent = ureq::AgentBuilder::new().redirects(0).build();

        let err = agent.get(&format!("http://127.0.0.1:{port}/api/session")).call();
        assert_eq!(err.unwrap_err().into_response().unwrap().status(), 401);

        let wrong_origin = agent
            .get(&format!("http://127.0.0.1:{port}/api/session"))
            .set("Origin", "https://attacker.example")
            .set(AUTH_HEADER_NAME, &server.auth_token)
            .call();
        assert_eq!(wrong_origin.unwrap_err().into_response().unwrap().status(), 403);

        let ok = agent
            .get(&format!("http://127.0.0.1:{port}/api/session"))
            .set(AUTH_HEADER_NAME, &server.auth_token)
            .call()
            .unwrap();
        assert_eq!(ok.status(), 200);
        assert!(ok.header("Access-Control-Allow-Origin").is_none());

        let with_cookie = agent
            .get(&format!("http://127.0.0.1:{port}/api/desktop/status"))
            .set("Cookie", &format!("{AUTH_COOKIE_NAME}={}", server.auth_token))
            .call()
            .unwrap();
        assert_eq!(with_cookie.status(), 200);
    }

    #[test]
    fn bootstrap_nonce_is_single_use_and_sets_cookie() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let server = LocalServer::start_with_store(store).unwrap();
        let boot = server.bootstrap_url("/");
        let agent = ureq::AgentBuilder::new().redirects(0).build();

        let first = agent.get(&boot).call().unwrap();
        assert_eq!(first.status(), 302);
        let set_cookie = first.header("Set-Cookie").unwrap_or_default();
        assert!(set_cookie.contains(AUTH_COOKIE_NAME));
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Strict"));

        let second = agent.get(&boot).call();
        assert_eq!(second.unwrap_err().into_response().unwrap().status(), 403);
    }

    #[test]
    fn unready_runtime_returns_truthful_setup_required_and_blocks_events_without_auth() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let server = LocalServer::start_with_store(store).unwrap();
        let port = server.port;
        let tok = &server.auth_token;
        let agent = ureq::AgentBuilder::new().redirects(0).build();

        let browser = agent
            .get(&format!("http://127.0.0.1:{port}/api/browser"))
            .set(AUTH_HEADER_NAME, tok)
            .call()
            .unwrap_err()
            .into_response()
            .unwrap();
        assert_eq!(browser.status(), 503);
        let body: Value = serde_json::from_str(&browser.into_string().unwrap()).unwrap();
        assert_eq!(body["ready"], json!(false));
        assert_eq!(body["connected"], json!(false));
        assert_eq!(body["status"], json!("setup_required"));

        for path in ["/api/apps", "/api/channels", "/api/vault", "/api/trust"] {
            let resp = agent
                .get(&format!("http://127.0.0.1:{port}{path}"))
                .set(AUTH_HEADER_NAME, tok)
                .call()
                .unwrap();
            assert_eq!(resp.status(), 200);
        }

        let cu_get = agent
            .get(&format!("http://127.0.0.1:{port}/api/computer-use"))
            .set(AUTH_HEADER_NAME, tok)
            .call()
            .unwrap();
        assert_eq!(cu_get.status(), 200);
        let cu_body: Value = serde_json::from_str(&cu_get.into_string().unwrap()).unwrap();
        assert_eq!(cu_body["status"], json!("setup_required"));
        assert_eq!(cu_body["ready"], json!(false));
        assert_eq!(cu_body["allowAutomaticRetry"], json!(false));

        let cu_patch = agent
            .request("PATCH", &format!("http://127.0.0.1:{port}/api/computer-use"))
            .set(AUTH_HEADER_NAME, tok)
            .set("Content-Type", "application/json")
            .send_string(&json!({ "enabled": true }).to_string())
            .unwrap_err()
            .into_response()
            .unwrap();
        assert_eq!(cu_patch.status(), 409);

        let events_unauth = agent
            .get(&format!("http://127.0.0.1:{port}/api/events"))
            .call()
            .unwrap_err()
            .into_response()
            .unwrap();
        assert_eq!(events_unauth.status(), 401);

        let events_bad_origin = agent
            .get(&format!("http://127.0.0.1:{port}/api/events"))
            .set("Origin", "https://evil.example")
            .set(AUTH_HEADER_NAME, tok)
            .call()
            .unwrap_err()
            .into_response()
            .unwrap();
        assert_eq!(events_bad_origin.status(), 403);
    }

    #[test]
    fn api_contract_shapes_prevent_react_ui_crashes() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = crate::api::session_state(&store);
        if let Some(obj) = session.as_object_mut() {
            obj.insert("onboardingCompleted".to_string(), json!(true));
        }
        store.set_setting("session", &session);
        store.set_setting("models", &json!({}));
        let server = LocalServer::start_with_store(store).unwrap();
        server.runtime.force_ready_for_test();
        let port = server.port;
        let tok = &server.auth_token;
        let agent = ureq::AgentBuilder::new().redirects(0).build();
        let post = agent
            .post(&format!("http://127.0.0.1:{port}/api/messages"))
            .set(AUTH_HEADER_NAME, tok)
            .set("Content-Type", "application/json")
            .send_string(&json!({ "text": "hello nod", "attachments": [] }).to_string())
            .unwrap();
        assert_eq!(post.status(), 200);

        let get_json = |path: &str| -> (u16, Value) {
            let resp = match agent
                .get(&format!("http://127.0.0.1:{port}{path}"))
                .set(AUTH_HEADER_NAME, tok)
                .call()
            {
                Ok(r) => r,
                Err(ureq::Error::Status(_, r)) => r,
                Err(err) => panic!("request failed for {path}: {err}"),
            };
            let status = resp.status();
            let body: Value = serde_json::from_str(&resp.into_string().unwrap()).unwrap();
            (status, body)
        };

        let (speech_status, speech) = get_json("/api/speech");
        assert_eq!(speech_status, 200);
        assert!(speech["voices"].is_array(), "VoiceSettings requires voices array: {speech}");
        assert!(speech["speechReply"].is_string());

        let (mem_status, memory) = get_json("/api/memory");
        assert_eq!(mem_status, 200);
        assert!(memory["notes"].is_array(), "Memory requires notes array: {memory}");
        assert!(memory["archive"].is_array(), "Memory requires archive array: {memory}");
        assert!(memory["skills"].is_array(), "Memory requires skills array: {memory}");

        let (cu_status, cu) = get_json("/api/computer-use");
        assert_eq!(cu_status, 200);
        assert!(cu["keptOut"].is_array());
        assert!(cu["platform"].is_string());
        assert!(cu["permissions"]["accessibility"].is_string(), "ComputerUseSettings requires permissions.accessibility: {cu}");
        assert!(cu["permissions"]["screen"].is_string());

        let (keep_status, keep_out) = get_json("/api/local-files/keep-out");
        assert_eq!(keep_status, 200);
        assert!(keep_out["paths"].is_array());

        let (usage_status, usage) = get_json("/api/usage/work");
        assert_eq!(usage_status, 200);
        assert!(usage["lanes"]["chat"].is_object());

        let (models_status, models) = get_json("/api/models");
        assert_eq!(models_status, 200);
        assert!(models["connections"].is_array());
        assert!(models["routes"].is_object());

        let (state_status, state) = get_json("/api/state");
        assert_eq!(state_status, 200);
        assert!(state["messages"].is_array());
        assert!(state["turns"].is_array());
        assert!(state["tasks"].is_array());

        let (ideas_status, ideas) = get_json("/api/schedules/ideas");
        assert_eq!(ideas_status, 200);
        assert!(ideas["ideas"].is_array());

        let (jobs_status, jobs) = get_json("/api/jobs/setup");
        assert_eq!(jobs_status, 200);
        assert!(jobs.is_object());

        let (bots_status, bots) = get_json("/api/bots");
        assert_eq!(bots_status, 200);
        assert_eq!(bots["mainBotId"], json!("main"));
        assert!(bots["bots"].is_array());

        let (apps_status, apps_list) = get_json("/api/apps");
        assert_eq!(apps_status, 200);
        assert!(apps_list.is_array());

        let (catalog_status, catalog) = get_json("/api/apps/catalog");
        assert_eq!(catalog_status, 200);
        assert!(catalog.is_array());

        let (vault_status, vault_items) = get_json("/api/vault");
        assert_eq!(vault_status, 200);
        assert!(vault_items.is_array());

        let (channels_status, channels) = get_json("/api/channels");
        assert_eq!(channels_status, 200);
        assert!(channels["channels"].is_array());

        let (ch_settings_status, ch_settings) = get_json("/api/channel-settings");
        assert_eq!(ch_settings_status, 200);
        assert_eq!(ch_settings["ready"], json!(true));
        assert!(ch_settings["channels"].is_array());

        let (trust_status, trust) = get_json("/api/trust");
        assert_eq!(trust_status, 200);
        assert!(trust["entries"].is_array());
        assert!(trust["week"].is_object());
        assert!(trust["month"].is_object());

        if let Some(root) = find_web_root() {
            assert!(root.join("setup.html").is_file());
            assert!(root.join("index.html").is_file());
            assert!(root.join("logos/models/hostinger.svg").is_file());
        }
    }

    #[test]
    fn nul_characters_are_rejected_with_bad_request() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let server = LocalServer::start_with_store(store).unwrap();
        let port = server.port;
        let tok = &server.auth_token;
        let agent = ureq::AgentBuilder::new().redirects(0).build();

        let err = agent
            .post(&format!("http://127.0.0.1:{port}/api/memory/notes"))
            .set(AUTH_HEADER_NAME, tok)
            .set("Content-Type", "application/json")
            .send_string(&json!({ "text": "bad\u{0000}note" }).to_string())
            .unwrap_err()
            .into_response()
            .unwrap();
        assert_eq!(err.status(), 400);

        let callback_err = agent
            .get(&format!("http://127.0.0.1:{port}/api/apps/callback?state=invalid-state&code=abc"))
            .call()
            .unwrap();
        assert_eq!(callback_err.status(), 302);
        let loc = callback_err.header("Location").unwrap_or("");
        assert!(loc.contains("/connected.html?connection=expired"), "unexpected Location: {loc}");

        let callback_no_state = agent
            .get(&format!("http://127.0.0.1:{port}/api/apps/callback"))
            .call()
            .unwrap_err()
            .into_response()
            .unwrap();
        assert_eq!(callback_no_state.status(), 401);
    }
}
