use super::{preferences_state, text, vault, Ctx, Reply};
use crate::db::Store;
use crate::util::{
    base64url_encode, iso, iso_now, parse_iso, random_base64url, sha256_hex, url_encode_component,
    uuid,
};
use chrono::Duration as ChronoDuration;
use ed25519_compact::{KeyPair, Seed};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LINEAR_PLUGIN_JSON: &str = include_str!("../../../src/apps/linear/plugin.json");
const LINEAR_APP_JSON: &str = include_str!("../../../src/apps/linear/io.n8n.nod/app.json");
const LINEAR_CONTRACT_JSON: &str = include_str!("../../../src/apps/linear/io.n8n.nod/contract.json");
const LINEAR_MCP_JSON: &str = include_str!("../../../src/apps/linear/mcp.json");
const LINEAR_ICON_SVG: &str = include_str!("../../../src/apps/linear/io.n8n.nod/icon.svg");

const N8N_PLUGIN_JSON: &str = include_str!("../../../src/apps/n8n/plugin.json");
const N8N_APP_JSON: &str = include_str!("../../../src/apps/n8n/io.n8n.nod/app.json");
const N8N_CONTRACT_JSON: &str = include_str!("../../../src/apps/n8n/io.n8n.nod/contract.json");
const N8N_ICON_SVG: &str = include_str!("../../../src/apps/n8n/io.n8n.nod/icon.svg");

const NOTION_PLUGIN_JSON: &str = include_str!("../../../src/apps/notion/plugin.json");
const NOTION_APP_JSON: &str = include_str!("../../../src/apps/notion/io.n8n.nod/app.json");
const NOTION_CONTRACT_JSON: &str = include_str!("../../../src/apps/notion/io.n8n.nod/contract.json");
const NOTION_MCP_JSON: &str = include_str!("../../../src/apps/notion/mcp.json");
const NOTION_ICON_SVG: &str = include_str!("../../../src/apps/notion/io.n8n.nod/icon.svg");

const COMPOSIO_API_BASE: &str = "https://backend.composio.dev/api/v3";
const COMPOSIO_PROXY_ORIGIN: &str = "https://composio.n8n.cloud";
const COMPOSIO_PRIVACY_NOTICE: &str = "<p>App data that Nod reads or sends passes through this connection service and Composio. This includes email content for Gmail and Outlook. Both services can read this content. By default, n8n runs the connection service.</p><p>Retention depends on the connection service's settings and Composio plan. When Zero Data Retention is on for the project, covered calls keep request arguments, response data and error text out of Composio execution logs. Composio still processes calls and keeps metadata. Calls through Composio Managed OAuth apps need Enterprise for this coverage. Files, triggers and tool searches have separate rules.</p><p><a href=\"https://nod.n8n.io/docs/trust/privacy/#composio-zero-data-retention\" target=\"_blank\" rel=\"noopener noreferrer\">Read about data handling and retention</a></p>";

#[derive(Clone, Debug)]
struct BundledAppDef {
    slug: String,
    name: String,
    description: String,
    categories: Vec<String>,
    endpoint: Option<String>,
    instance_path: Option<String>,
    auth: Vec<String>,
    scope: Option<String>,
    hints: Value,
    icon_svg: &'static str,
}

#[derive(Clone, Debug)]
pub struct ConnectedMcpToolSpec {
    pub connection_id: String,
    pub service: String,
    pub app_name: String,
    pub tool_name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug)]
struct ParsedUrl {
    scheme: String,
    host: String,
    port: Option<u16>,
    path: String,
    has_query: bool,
    has_fragment: bool,
    has_userinfo: bool,
    origin: String,
    normalized: String,
}

struct ComposioCatalogCache {
    fetched_at: Instant,
    items: Vec<Value>,
}

#[derive(Clone, Debug)]
struct ProxyConnectAttempt {
    capability: String,
    transaction_id: String,
    discovery_id: String,
    slug: String,
    challenge: String,
    bridge_key: String,
    origin: String,
    expires_at: Instant,
    bootstrapped: bool,
    starting: bool,
}

static COMPOSIO_CACHE: OnceLock<Mutex<Option<ComposioCatalogCache>>> = OnceLock::new();
static BUNDLED_CACHE: OnceLock<Vec<BundledAppDef>> = OnceLock::new();
static PROXY_ENROLLED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static PROXY_CLOCK_OFFSET: OnceLock<Mutex<i64>> = OnceLock::new();
static PROXY_ATTEMPTS: OnceLock<Mutex<HashMap<String, ProxyConnectAttempt>>> = OnceLock::new();
static PROXY_SESSIONS: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();

fn composio_cache() -> &'static Mutex<Option<ComposioCatalogCache>> {
    COMPOSIO_CACHE.get_or_init(|| Mutex::new(None))
}

fn proxy_enrolled() -> &'static Mutex<HashSet<String>> {
    PROXY_ENROLLED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn proxy_clock_offset() -> &'static Mutex<i64> {
    PROXY_CLOCK_OFFSET.get_or_init(|| Mutex::new(0))
}

fn proxy_attempts() -> &'static Mutex<HashMap<String, ProxyConnectAttempt>> {
    PROXY_ATTEMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn proxy_sessions() -> &'static Mutex<HashMap<String, (String, Instant)>> {
    PROXY_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn composio_proxy_origin() -> Option<String> {
    if let Ok(url) = std::env::var("NOD_COMPOSIO_PROXY_URL") {
        let trimmed = url.trim().trim_end_matches('/').to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    if cfg!(test) {
        None
    } else {
        Some(COMPOSIO_PROXY_ORIGIN.to_string())
    }
}

fn validated_proxy_origin(origin: &str) -> Result<String, String> {
    check_public_https_url(origin, false, true)
        .map(|_| origin.to_string())
        .map_err(|e| format!("App connection service address is not allowed: {e}"))
}

fn decode_hex_32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn encode_hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct ProxyIdentity {
    keypair: KeyPair,
    public_key: String,
    user_id: String,
}

fn load_or_create_proxy_identity(store: &Store) -> Result<ProxyIdentity, String> {
    if let Some(saved) = store.setting("composio_proxy_identity") {
        let enc = saved
            .get("seedEnc")
            .and_then(Value::as_str)
            .ok_or_else(|| "The app connection identity is incomplete and was left unchanged.".to_string())?;
        let plain = vault::decrypt_secret(enc)
            .map_err(|e| format!("Could not unlock the app connection identity: {e}"))?;
        let seed_bytes = decode_hex_32(plain.trim())
            .ok_or_else(|| "The app connection identity is damaged and was left unchanged.".to_string())?;
        let keypair = KeyPair::from_seed(Seed::new(seed_bytes));
        let pk_bytes = keypair.pk.as_ref();
        let public_key = base64url_encode(pk_bytes);
        let user_id = format!("nod_{}", sha256_hex(pk_bytes));
        return Ok(ProxyIdentity {
            keypair,
            public_key,
            user_id,
        });
    }

    let mut seed_bytes = [0u8; 32];
    getrandom::fill(&mut seed_bytes).map_err(|e| format!("Could not generate app identity: {e}"))?;
    let keypair = KeyPair::from_seed(Seed::new(seed_bytes));
    let pk_bytes = keypair.pk.as_ref();
    let public_key = base64url_encode(pk_bytes);
    let user_id = format!("nod_{}", sha256_hex(pk_bytes));
    let seed_enc = vault::encrypt_secret(&encode_hex_bytes(&seed_bytes))?;
    store.set_setting(
        "composio_proxy_identity",
        &json!({
            "publicKey": public_key,
            "userId": user_id,
            "seedEnc": seed_enc,
        }),
    );
    Ok(ProxyIdentity {
        keypair,
        public_key,
        user_id,
    })
}

fn proxy_request_once(
    agent: &ureq::Agent,
    identity: &ProxyIdentity,
    origin: &str,
    method: &str,
    path_and_query: &str,
    body_bytes: &[u8],
) -> Result<(u16, Option<String>, String), String> {
    let offset = proxy_clock_offset().lock().map(|g| *g).unwrap_or(0);
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + offset;
    let timestamp = now_secs.to_string();
    let request_id = uuid();
    let body_hash = sha256_hex(body_bytes);
    let canonical = format!(
        "nod-composio-v1\n{method}\n{origin}\n{path_and_query}\n{timestamp}\n{request_id}\n{body_hash}"
    );
    let sig = identity.keypair.sk.sign(canonical.as_bytes(), None);
    let sig_b64 = base64url_encode(sig.as_ref());

    let url = format!("{origin}{path_and_query}");
    let mut req = agent
        .request(method, &url)
        .set("Accept", "application/json")
        .set("x-nod-public-key", &identity.public_key)
        .set("x-nod-timestamp", &timestamp)
        .set("x-nod-request-id", &request_id)
        .set("x-nod-signature", &sig_b64);
    if !body_bytes.is_empty() {
        req = req.set("Content-Type", "application/json");
    }

    let resp = if body_bytes.is_empty() {
        req.call()
    } else {
        req.send_bytes(body_bytes)
    };
    match resp {
        Ok(r) => {
            let status = r.status();
            let date_hdr = r.header("date").map(str::to_string);
            let txt = r.into_string().map_err(|e| format!("Could not read proxy response: {e}"))?;
            Ok((status, date_hdr, txt))
        }
        Err(ureq::Error::Status(status, r)) => {
            let date_hdr = r.header("date").map(str::to_string);
            let txt = r.into_string().unwrap_or_default();
            Ok((status, date_hdr, txt))
        }
        Err(ureq::Error::Transport(t)) => Err(format!("Could not reach app connection service: {t}")),
    }
}

fn proxy_signed_request(
    store: &Store,
    origin: &str,
    method: &str,
    path_and_query: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    let origin = validated_proxy_origin(origin)?;
    let identity = load_or_create_proxy_identity(store)?;
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .build();
    let payload_bytes = match body {
        Some(v) => v.to_string().into_bytes(),
        None => Vec::new(),
    };

    if !path_and_query.starts_with("/v2/access/") {
        let enrollment_key = format!("{origin}|{}", identity.public_key);
        let already = proxy_enrolled()
            .lock()
            .map(|g| g.contains(&enrollment_key))
            .unwrap_or(false);
        if !already {
            let enrolled = proxy_signed_request(store, &origin, "POST", "/v2/access/enroll", Some(&json!({})))?;
            if enrolled.get("state").and_then(Value::as_str) != Some("enabled")
                || enrolled.get("userId").and_then(Value::as_str) != Some(identity.user_id.as_str())
            {
                return Err("App connection service enrollment could not be verified.".to_string());
            }
            if let Ok(mut guard) = proxy_enrolled().lock() {
                guard.insert(enrollment_key);
            }
        }
    }

    let (mut status, mut date_hdr, mut text_body) =
        proxy_request_once(&agent, &identity, &origin, method, path_and_query, &payload_bytes)?;
    if status == 401
        && let Ok(parsed) = serde_json::from_str::<Value>(&text_body)
        && parsed.get("error").and_then(Value::as_str) == Some("expired_authentication")
        && let Some(date_str) = date_hdr.as_deref()
        && let Ok(dt) = chrono::DateTime::parse_from_rfc2822(date_str)
    {
        let local_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Ok(mut g) = proxy_clock_offset().lock() {
            *g = dt.timestamp() - local_secs;
        }
        (status, date_hdr, text_body) =
            proxy_request_once(&agent, &identity, &origin, method, path_and_query, &payload_bytes)?;
    }
    let _ = date_hdr;

    if !(200..300).contains(&status) {
        let detail = serde_json::from_str::<Value>(&text_body)
            .ok()
            .and_then(|v| {
                v.get("message")
                    .or_else(|| v.get("error"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| text_body.chars().take(240).collect());
        return Err(format!("App connection service returned HTTP {status}: {detail}"));
    }

    serde_json::from_str(&text_body)
        .map_err(|_| "App connection service returned invalid JSON.".to_string())
}

fn composio_api_key(store: &Store) -> Option<String> {
    if let Ok(key) = std::env::var("COMPOSIO_API_KEY") {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    store
        .setting("composio_key")
        .and_then(|v| v.get("keyEnc").and_then(Value::as_str).map(str::to_string))
        .and_then(|enc| vault::decrypt_secret(&enc).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_url(raw: &str) -> Result<ParsedUrl, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 2048 || trimmed.chars().any(char::is_control) {
        return Err("Enter a valid URL.".to_string());
    }
    let (scheme_raw, rest) = trimmed
        .split_once("://")
        .ok_or_else(|| "URL must start with https:// or http://.".to_string())?;
    let scheme = scheme_raw.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return Err("Only HTTPS and HTTP URLs are supported.".to_string());
    }
    let (before_frag, has_fragment) = match rest.split_once('#') {
        Some((prefix, _)) => (prefix, true),
        None => (rest, false),
    };
    let path_idx = before_frag.find(['/', '?']);
    let authority = match path_idx {
        Some(idx) => &before_frag[..idx],
        None => before_frag,
    };
    let tail = match path_idx {
        Some(idx) => &before_frag[idx..],
        None => "/",
    };
    let (authority_clean, has_userinfo) = match authority.rsplit_once('@') {
        Some((_, h)) => (h, true),
        None => (authority, false),
    };
    if authority_clean.is_empty() {
        return Err("Enter a valid server hostname.".to_string());
    }
    let (host, port) = if let Some(inner) = authority_clean.strip_prefix('[') {
        let end = inner
            .find(']')
            .ok_or_else(|| "Invalid IPv6 host.".to_string())?;
        let ip_part = &inner[..end];
        let after = &inner[end + 1..];
        let port = if let Some(p) = after.strip_prefix(':') {
            Some(p.parse::<u16>().map_err(|_| "Invalid URL port.".to_string())?)
        } else if after.is_empty() {
            None
        } else {
            return Err("Invalid URL authority.".to_string());
        };
        (format!("[{ip_part}]"), port)
    } else if let Some((h, p)) = authority_clean.rsplit_once(':') {
        let port_num = p.parse::<u16>().map_err(|_| "Invalid URL port.".to_string())?;
        (h.to_ascii_lowercase(), Some(port_num))
    } else {
        (authority_clean.to_ascii_lowercase(), None)
    };
    if host.is_empty() || host.contains('\\') {
        return Err("Enter a valid server hostname.".to_string());
    }
    let (path, has_query) = match tail.split_once('?') {
        Some((p, _)) => (if p.is_empty() { "/" } else { p }, true),
        None => (tail, false),
    };
    let path_and_query = if tail.starts_with('/') {
        tail.to_string()
    } else {
        format!("/{tail}")
    };
    let authority_norm = match port {
        Some(p)
            if !((scheme == "https" && p == 443) || (scheme == "http" && p == 80)) =>
        {
            format!("{host}:{p}")
        }
        _ => host.clone(),
    };
    let origin = format!("{scheme}://{authority_norm}");
    let normalized = format!("{origin}{path_and_query}");
    Ok(ParsedUrl {
        scheme,
        host,
        port,
        path: path.to_string(),
        has_query,
        has_fragment,
        has_userinfo,
        origin,
        normalized,
    })
}

fn is_disallowed_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (o[0] == 100 && (o[1] & 0b1100_0000) == 64)
        || (o[0] == 0)
}

fn is_disallowed_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_disallowed_ipv4(v4);
    }
    let seg = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (seg[0] & 0xfe00) == 0xfc00
        || (seg[0] & 0xffc0) == 0xfe80
}

fn is_disallowed_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_disallowed_ipv4(v4),
        IpAddr::V6(v6) => is_disallowed_ipv6(v6),
    }
}

fn is_loopback_hostname(host: &str) -> bool {
    let bare = host.trim_matches(|c| c == '[' || c == ']').trim_end_matches('.');
    if bare.eq_ignore_ascii_case("localhost") {
        return true;
    }
    bare.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

fn is_reserved_hostname(host: &str) -> bool {
    let clean = host.trim_end_matches('.').to_ascii_lowercase();
    clean == "localhost"
        || clean == "local"
        || clean == "internal"
        || clean == "invalid"
        || clean == "test"
        || clean == "example.com"
        || clean == "example.org"
        || clean == "example.net"
        || clean.ends_with(".localhost")
        || clean.ends_with(".local")
        || clean.ends_with(".internal")
        || clean.ends_with(".invalid")
        || clean.ends_with(".test")
        || clean.ends_with(".example.com")
        || clean.ends_with(".example.org")
        || clean.ends_with(".example.net")
}

fn check_public_https_url(raw: &str, allow_query: bool, allow_local_loopback: bool) -> Result<(ParsedUrl, bool), String> {
    let parsed = parse_url(raw)?;
    if parsed.has_userinfo || parsed.has_fragment {
        return Err("Remove credentials or fragments from the server address.".to_string());
    }
    if !allow_query && parsed.has_query {
        return Err("Use a fixed address without a query string or fragment.".to_string());
    }
    let loopback = is_loopback_hostname(&parsed.host);
    if loopback && allow_local_loopback {
        return Ok((parsed, true));
    }
    if parsed.scheme != "https" {
        return Err("Use a public HTTPS server address.".to_string());
    }
    if let Some(p) = parsed.port
        && p != 443
    {
        return Err("Public HTTPS endpoints must use the standard port 443.".to_string());
    }
    let bare_host = parsed.host.trim_matches(|c| c == '[' || c == ']');
    if bare_host.parse::<IpAddr>().is_ok() || is_reserved_hostname(bare_host) {
        return Err("Use a fixed public HTTPS address rather than an IP or local/example domain.".to_string());
    }
    if let Ok(addrs) = (bare_host, 443_u16).to_socket_addrs() {
        for addr in addrs {
            if is_disallowed_ip(addr.ip()) {
                return Err("Private or local network destinations are blocked.".to_string());
            }
        }
    }
    Ok((parsed, false))
}

fn check_manifest_app(
    expected_slug: &str,
    plugin_raw: &str,
    app_raw: &str,
    contract_raw: &str,
    mcp_raw: Option<&str>,
    icon_svg: &'static str,
) -> Option<BundledAppDef> {
    let plugin: Value = serde_json::from_str(plugin_raw).ok()?;
    let app: Value = serde_json::from_str(app_raw).ok()?;
    let contract: Value = serde_json::from_str(contract_raw).ok()?;

    let slug = plugin.get("name")?.as_str()?;
    if slug != expected_slug {
        return None;
    }
    let description = plugin.get("description")?.as_str()?.trim().to_string();
    if description.is_empty() || description.len() > 1000 {
        return None;
    }
    if app.get("$schema")?.as_str()? != "https://nod.n8n.io/docs/schemas/apps/v1/app.schema.json" {
        return None;
    }
    if contract.get("$schema")?.as_str()? != "https://nod.n8n.io/docs/schemas/apps/v1/contract.schema.json" {
        return None;
    }
    let name = app.get("displayName")?.as_str()?.trim().to_string();
    if name.is_empty() || name.len() > 80 {
        return None;
    }
    let categories = app
        .get("categories")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let conn = app.get("connection")?;
    let auth = conn
        .get("authentication")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let scope = conn.get("scopes").and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    });
    let hints = conn.get("hints").cloned().unwrap_or_else(|| json!({}));

    let (endpoint, instance_path) = if let Some(mcp_text) = mcp_raw {
        let server_name = conn.get("server")?.as_str()?;
        let mcp: Value = serde_json::from_str(mcp_text).ok()?;
        let server_cfg = mcp.get("mcpServers")?.get(server_name)?;
        if server_cfg.get("type")?.as_str()? != "streamable-http" {
            return None;
        }
        let raw_url = server_cfg.get("url")?.as_str()?;
        let parsed = parse_url(raw_url).ok()?;
        if parsed.scheme != "https" || parsed.has_query || parsed.has_fragment || parsed.has_userinfo {
            return None;
        }
        (Some(parsed.normalized), None)
    } else {
        let path = conn.pointer("/endpoint/path")?.as_str()?.to_string();
        (None, Some(path))
    };

    if icon_svg.is_empty() || icon_svg.len() > 128 * 1024 {
        return None;
    }

    Some(BundledAppDef {
        slug: slug.to_string(),
        name,
        description,
        categories,
        endpoint,
        instance_path,
        auth,
        scope,
        hints,
        icon_svg,
    })
}

fn bundled_apps() -> &'static [BundledAppDef] {
    BUNDLED_CACHE.get_or_init(|| {
        let mut list = Vec::new();
        if let Some(def) = check_manifest_app(
            "linear",
            LINEAR_PLUGIN_JSON,
            LINEAR_APP_JSON,
            LINEAR_CONTRACT_JSON,
            Some(LINEAR_MCP_JSON),
            LINEAR_ICON_SVG,
        ) {
            list.push(def);
        }
        if let Some(def) = check_manifest_app(
            "n8n",
            N8N_PLUGIN_JSON,
            N8N_APP_JSON,
            N8N_CONTRACT_JSON,
            None,
            N8N_ICON_SVG,
        ) {
            list.push(def);
        }
        if let Some(def) = check_manifest_app(
            "notion",
            NOTION_PLUGIN_JSON,
            NOTION_APP_JSON,
            NOTION_CONTRACT_JSON,
            Some(NOTION_MCP_JSON),
            NOTION_ICON_SVG,
        ) {
            list.push(def);
        }
        list
    })
}

fn bundled_app(slug: &str) -> Option<&'static BundledAppDef> {
    bundled_apps().iter().find(|item| item.slug == slug)
}

fn is_valid_slug(slug: &str) -> bool {
    let bytes = slug.as_bytes();
    if bytes.is_empty() || bytes.len() > 80 {
        return false;
    }
    bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

fn is_valid_app_name(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty()
        && trimmed.chars().count() <= 80
        && !trimmed.chars().any(char::is_control)
}

fn is_manual_mcp_service(service: &str) -> bool {
    let Some(hex) = service.strip_prefix("mcp-") else {
        return false;
    };
    hex.len() == 24 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_valid_composio_logo(url: &str) -> bool {
    url.len() <= 2048
        && (url.starts_with("https://logos.composio.dev/api/")
            || url.starts_with("https://assets.composio.dev/logos/"))
}

fn parse_composio_toolkit_items(items: &[Value], out: &mut Vec<Value>) {
    for entry in items {
        let slug = entry
            .get("slug")
            .and_then(Value::as_str)
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if !is_valid_slug(&slug) || slug == "notion_mcp" {
            continue;
        }
        if entry.get("no_auth").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let auth_schemes = entry
            .get("auth_schemes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_ascii_uppercase())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !auth_schemes.is_empty() && auth_schemes.iter().all(|s| s == "NO_AUTH") {
            continue;
        }
        let managed = entry
            .get("composio_managed_auth_schemes")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if !managed && !auth_schemes.iter().any(|s| s == "DCR_OAUTH") {
            continue;
        }
        let Some(name) = entry
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 200)
        else {
            continue;
        };
        let description = entry
            .pointer("/meta/description")
            .and_then(Value::as_str)
            .map(|s| s.chars().take(1000).collect::<String>())
            .unwrap_or_default();
        let logo = entry
            .pointer("/meta/logo")
            .and_then(Value::as_str)
            .filter(|u| is_valid_composio_logo(u));
        let categories: Vec<String> = entry
            .pointer("/meta/categories")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .take(32)
                    .filter_map(|c| {
                        c.get("name")
                            .and_then(Value::as_str)
                            .or_else(|| c.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty() && s.len() <= 80)
                            .map(str::to_string)
                    })
                    .take(12)
                    .collect()
            })
            .unwrap_or_default();

        let mut item = json!({
            "slug": slug,
            "name": name,
            "description": description,
            "categories": categories,
            "status": "disconnected",
            "triggersEnabled": false,
            "supportsTriggers": false,
        });
        if let Some(logo_url) = logo {
            item["logoUrl"] = json!(logo_url);
        }
        out.push(item);
    }
}

fn fetch_composio_pages<F>(fetch_page: F) -> Result<Vec<Value>, String>
where
    F: Fn(&str) -> Result<Value, String>,
{
    if let Ok(guard) = composio_cache().lock()
        && let Some(cached) = guard.as_ref()
        && cached.fetched_at.elapsed() < Duration::from_secs(15 * 60)
    {
        return Ok(cached.items.clone());
    }

    let mut result = Vec::new();
    let mut seen_cursors = HashSet::new();
    let mut cursor: Option<String> = None;

    for _ in 0..10 {
        let mut query = String::from("type=native&include_deprecated=false&limit=1000");
        if let Some(c) = cursor.as_deref() {
            query.push_str("&cursor=");
            query.push_str(&url_encode_component(c));
        }
        let parsed = fetch_page(&query)?;
        let Some(items) = parsed.get("items").and_then(Value::as_array) else {
            return Err("The app catalog response did not include a toolkit list.".to_string());
        };
        parse_composio_toolkit_items(items, &mut result);
        cursor = parsed
            .get("next_cursor")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(ref c) = cursor {
            if !seen_cursors.insert(c.clone()) {
                break;
            }
        } else {
            break;
        }
    }

    if cursor.is_some() {
        return Err("The app catalog did not finish loading every page.".to_string());
    }

    if let Ok(mut guard) = composio_cache().lock() {
        *guard = Some(ComposioCatalogCache {
            fetched_at: Instant::now(),
            items: result.clone(),
        });
    }
    Ok(result)
}

fn fetch_composio_catalog(api_key: &str) -> Result<Vec<Value>, String> {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(6))
        .timeout(Duration::from_secs(10))
        .build();
    fetch_composio_pages(|query| {
        let url = format!("{COMPOSIO_API_BASE}/toolkits?{query}");
        let resp = agent
            .get(&url)
            .set("x-api-key", api_key)
            .set("Accept", "application/json")
            .call()
            .map_err(|e| format!("Could not load Composio catalog: {e}"))?;
        let text_body = resp
            .into_string()
            .map_err(|e| format!("Could not read Composio catalog response: {e}"))?;
        serde_json::from_str(&text_body)
            .map_err(|_| "Composio returned invalid JSON for catalog.".to_string())
    })
}

fn fetch_composio_proxy_catalog(store: &Store, proxy_origin: &str) -> Result<Vec<Value>, String> {
    fetch_composio_pages(|query| {
        let path_and_query = format!("/v2/api/v3.1/toolkits?{query}");
        proxy_signed_request(store, proxy_origin, "GET", &path_and_query, None)
    })
}

pub fn handle(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    match (method, path) {
        ("GET", "/api/apps") => Some(list_connected(ctx)),
        ("GET", "/api/apps/catalog") => Some(catalog(ctx)),
        ("POST", "/api/apps/mcp") => Some(create_mcp_discovery(ctx)),
        ("POST", "/api/apps/setup-status") => Some(setup_status(ctx)),
        ("POST", "/api/apps/setup-return") => Some(setup_return(ctx)),
        ("POST", "/api/apps/setup-cancel") => Some(setup_cancel(ctx)),
        ("GET", "/api/apps/callback") => Some(oauth_callback(ctx)),
        ("POST", "/api/apps/provider") => Some(save_composio_key(ctx)),
        ("GET", "/api/speech") | ("POST", "/api/speech/prepare") | ("POST", "/api/speech/voices/refresh") => {
            Some(speech(ctx))
        }
        ("POST", "/api/speech/preview") => Some(Reply::error(
            501,
            "Voice preview is not available in the native build.",
        )),
        _ => dispatch_dynamic(ctx),
    }
}

fn dispatch_dynamic(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    if let Some(capability) = path.strip_prefix("/api/apps/connect/") {
        return Some(proxy_connect_bridge(ctx, capability));
    }

    if let Some(rest) = path.strip_prefix("/api/apps/catalog/")
        && let Some(slug) = rest.strip_suffix("/icon")
        && method == "GET"
    {
        return Some(catalog_icon(slug));
    }

    if let Some(rest) = path.strip_prefix("/api/apps/discovery/") {
        let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        return match (method, parts.as_slice()) {
            ("GET", [id]) => Some(get_discovery(ctx, id)),
            ("POST", [id, "instance"]) => Some(set_discovery_instance(ctx, id)),
            ("POST", [id, "authorize"]) => Some(authorize_discovery(ctx, id)),
            ("POST", [id, "complete"]) => Some(complete_discovery(ctx, id)),
            _ => None,
        };
    }

    if let Some(service) = path.strip_prefix("/api/apps/mcp/")
        && method == "PATCH"
    {
        return Some(rename_mcp(ctx, service.trim_matches('/')));
    }

    let rest = path.strip_prefix("/api/apps/")?;
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match (method, parts.as_slice()) {
        ("GET", [id, "icon"]) => Some(app_icon(ctx, id)),
        ("POST", [slug, "connect"]) => Some(connect_catalog_app(ctx, slug)),
        ("POST", [slug, "accounts", conn_id, "disconnect"]) => {
            Some(disconnect_account(ctx, slug, conn_id))
        }
        ("PUT", [slug, "triggers"]) | ("PATCH", [slug, "triggers"]) => {
            Some(toggle_triggers(ctx, slug))
        }
        _ => None,
    }
}

fn notify_apps(ctx: &Ctx) {
    ctx.live.notify("apps");
    ctx.refresh();
}

fn public_connection(doc: &Value) -> Value {
    let mut view = doc.clone();
    if let Some(obj) = view.as_object_mut() {
        obj.remove("tokenEnc");
    }
    view
}

fn list_connected(ctx: &Ctx) -> Reply {
    let list: Vec<Value> = ctx
        .store
        .all("apps")
        .into_iter()
        .filter(|doc| {
            !doc.get("accessPaths")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().any(|p| text(p, "kind") == "browser"))
                .unwrap_or(false)
        })
        .map(|d| public_connection(&d))
        .collect();
    Reply::ok(Value::Array(list))
}

fn catalog_icon(slug: &str) -> Reply {
    if let Some(app) = bundled_app(slug) {
        return Reply::bytes(200, app.icon_svg.as_bytes().to_vec(), "image/svg+xml".to_string());
    }
    Reply::error(404, "The app icon is unavailable.")
}

fn app_icon(ctx: &Ctx, id: &str) -> Reply {
    if let Some(conn) = ctx.store.get("apps", id) {
        let slug = text(&conn, "slug");
        if let Some(app) = bundled_app(&slug) {
            return Reply::bytes(200, app.icon_svg.as_bytes().to_vec(), "image/svg+xml".to_string());
        }
    }
    Reply::error(404, "The app icon is unavailable.")
}

fn save_composio_key(ctx: &Ctx) -> Reply {
    let key = text(&ctx.body, "apiKey").trim().to_string();
    if key.is_empty() {
        ctx.store.delete("settings", "composio_key");
        if let Ok(mut guard) = composio_cache().lock() {
            *guard = None;
        }
        notify_apps(ctx);
        return Reply::ok(json!({ "configured": composio_api_key(ctx.store).is_some() }));
    }
    let enc = match vault::encrypt_secret(&key) {
        Ok(v) => v,
        Err(err) => return Reply::error(500, err),
    };
    ctx.store.set_setting("composio_key", &json!({ "keyEnc": enc }));
    if let Ok(mut guard) = composio_cache().lock() {
        *guard = None;
    }
    notify_apps(ctx);
    Reply::ok(json!({ "configured": true }))
}

fn catalog(ctx: &Ctx) -> Reply {
    let connected_slugs: HashSet<String> = ctx
        .store
        .all("apps")
        .into_iter()
        .map(|a| text(&a, "slug"))
        .collect();

    let mut by_slug: HashMap<String, Value> = HashMap::new();
    if let Some(api_key) = composio_api_key(ctx.store) {
        match fetch_composio_catalog(&api_key) {
            Ok(items) => {
                for mut item in items {
                    let slug = text(&item, "slug");
                    if connected_slugs.contains(&slug) {
                        item["status"] = json!("connected");
                    }
                    by_slug.insert(slug, item);
                }
            }
            Err(err) => return Reply::error(502, err),
        }
    } else if let Some(proxy_origin) = composio_proxy_origin() {
        match fetch_composio_proxy_catalog(ctx.store, &proxy_origin) {
            Ok(items) => {
                for mut item in items {
                    let slug = text(&item, "slug");
                    if connected_slugs.contains(&slug) {
                        item["status"] = json!("connected");
                    }
                    by_slug.insert(slug, item);
                }
            }
            Err(err) => return Reply::error(502, err),
        }
    }

    for def in bundled_apps() {
        let status = if connected_slugs.contains(&def.slug) {
            "connected"
        } else {
            "disconnected"
        };
        by_slug.insert(
            def.slug.clone(),
            json!({
                "slug": def.slug,
                "name": def.name,
                "description": def.description,
                "logoUrl": format!("/api/apps/catalog/{}/icon", def.slug),
                "categories": def.categories,
                "status": status,
                "direct": true,
                "triggersEnabled": false,
                "supportsTriggers": false,
            }),
        );
    }

    let mut entries: Vec<Value> = by_slug.into_values().collect();
    entries.sort_by_key(|a| text(a, "name"));
    Reply::ok(Value::Array(entries))
}

fn setup_status(ctx: &Ctx) -> Reply {
    let hashes: HashSet<String> = ctx
        .body
        .get("hashes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let conn_ids: HashSet<String> = ctx
        .body
        .get("connectionIds")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut completed = Vec::new();
    let mut apps_out = Vec::new();

    for disc in ctx.store.all("app_discoveries") {
        let id = text(&disc, "id");
        let proposal_hash = sha256_hex(format!("/apps?connect={id}").as_bytes());
        let link_hash = text(&disc, "linkHash");
        let status = text(&disc, "oauthStatus");
        let slug = text(&disc, "service");
        let name = text(&disc, "name");
        if status == "connected" {
            if hashes.contains(&proposal_hash) {
                completed.push(json!(proposal_hash));
            }
            if !link_hash.is_empty() && hashes.contains(&link_hash) {
                completed.push(json!(link_hash));
            }
            continue;
        }
        let matching_hash = if !link_hash.is_empty() && hashes.contains(&link_hash) {
            Some(link_hash)
        } else if hashes.contains(&proposal_hash) {
            Some(proposal_hash)
        } else {
            None
        };
        if let Some(hash) = matching_hash {
            let expired = parse_iso(&text(&disc, "expiresAt"))
                .map(|exp| exp <= crate::util::now())
                .unwrap_or(false);
            let mut row = json!({
                "hash": hash,
                "slug": slug,
                "name": name,
                "available": !expired && status != "failed" && status != "cancelled",
            });
            if let Some(err) = disc.get("oauthError").and_then(Value::as_str) {
                row["error"] = json!(err);
            }
            apps_out.push(row);
        }
    }

    for conn in ctx.store.all("apps") {
        let id = text(&conn, "id");
        let link_hash = text(&conn, "setupLinkHash");
        if !link_hash.is_empty() && hashes.contains(&link_hash) && text(&conn, "status") == "connected" {
            completed.push(json!(link_hash));
        } else if conn_ids.contains(&id) {
            apps_out.push(public_connection(&conn));
        }
    }

    Reply::ok(json!({
        "completed": completed,
        "apps": apps_out,
    }))
}

fn setup_return(ctx: &Ctx) -> Reply {
    let hash = text(&ctx.body, "hash");
    let return_to = text(&ctx.body, "returnTo");
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || return_to != "desktop" {
        return Reply::error(400, "Invalid setup return payload.");
    }
    for mut disc in ctx.store.all("app_discoveries") {
        if text(&disc, "linkHash") == hash {
            let id = text(&disc, "id");
            if let Some(obj) = disc.as_object_mut() {
                obj.insert("callbackReturnTo".to_string(), json!(return_to));
            }
            let _ = ctx.store.put("app_discoveries", &id, &disc);
            return Reply::ok(json!({ "ok": true }));
        }
    }
    Reply::error(409, "This sign-in link is no longer available.")
}

fn setup_cancel(ctx: &Ctx) -> Reply {
    let hash = text(&ctx.body, "hash");
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Reply::error(400, "Invalid setup cancel hash.");
    }
    for mut disc in ctx.store.all("app_discoveries") {
        if text(&disc, "linkHash") == hash && text(&disc, "oauthStatus") == "pending" {
            let id = text(&disc, "id");
            if let Some(obj) = disc.as_object_mut() {
                obj.insert("oauthStatus".to_string(), json!("cancelled"));
                obj.insert("oauthStateHash".to_string(), Value::Null);
            }
            let _ = ctx.store.put("app_discoveries", &id, &disc);
            ctx.store.delete("app_discovery_grants", &id);
        }
    }
    Reply::ok(json!({ "ok": true }))
}

fn build_bundled_discovery(def: &BundledAppDef, return_to: &str) -> Value {
    let id = uuid();
    let expires_at = iso(crate::util::now() + ChronoDuration::minutes(15));
    let paths = if let Some(ref endpoint) = def.endpoint {
        let mut path = json!({
            "kind": "mcp",
            "endpoint": endpoint,
            "auth": def.auth.first().map(|s| if s == "oauth2" { "oauth" } else { s.as_str() }).unwrap_or("oauth"),
        });
        if let Some(ref scope) = def.scope {
            path["scope"] = json!(scope);
        }
        vec![path]
    } else {
        Vec::new()
    };
    json!({
        "id": id,
        "service": def.slug,
        "name": def.name,
        "catalogApp": true,
        "needsAddress": def.instance_path.is_some(),
        "instancePath": def.instance_path,
        "localAddress": Value::Null,
        "hints": def.hints,
        "oauthStatus": Value::Null,
        "oauthError": Value::Null,
        "callbackReturnTo": return_to,
        "paths": paths,
        "expiresAt": expires_at,
    })
}

fn connect_catalog_app(ctx: &Ctx, slug: &str) -> Reply {
    if !is_valid_slug(slug) {
        return Reply::error(400, "Invalid app identifier.");
    }
    let return_to = match text(&ctx.body, "returnTo").as_str() {
        "onboarding" => "onboarding",
        "window" => "window",
        "desktop" => "desktop",
        _ => "apps",
    };
    if let Some(def) = bundled_app(slug) {
        let discovery = build_bundled_discovery(def, return_to);
        let id = text(&discovery, "id");
        if let Err(err) = ctx.store.put("app_discoveries", &id, &discovery) {
            return Reply::error(500, err);
        }
        return Reply::ok(json!({
            "url": format!("/apps?connect={id}"),
        }));
    }

    if let Some(api_key) = composio_api_key(ctx.store) {
        let state = random_base64url(32);
        let callback_url = format!(
            "{}/api/apps/callback?state={}",
            ctx.live.origin(),
            url_encode_component(&state)
        );
        return match start_composio_link(&api_key, slug, &callback_url, ctx.owner) {
            Ok((redirect_url, connected_account_id)) => {
                let id = uuid();
                let state_hash = sha256_hex(state.as_bytes());
                let link_hash = sha256_hex(redirect_url.as_bytes());
                let expires_at = iso(crate::util::now() + ChronoDuration::minutes(10));
                let record = json!({
                    "id": id,
                    "service": slug,
                    "name": slug,
                    "kind": "composio",
                    "composioAccountId": connected_account_id,
                    "oauthStatus": "pending",
                    "oauthStateHash": state_hash,
                    "linkHash": link_hash,
                    "callbackReturnTo": return_to,
                    "expiresAt": expires_at,
                });
                if let Err(err) = ctx.store.put("app_discoveries", &id, &record) {
                    return Reply::error(500, err);
                }
                Reply::ok(json!({
                    "url": redirect_url,
                    "attempt": state,
                }))
            }
            Err(err) => Reply::error(502, err),
        };
    }

    let Some(proxy_origin) = composio_proxy_origin() else {
        return Reply::error(
            503,
            "Configure COMPOSIO_API_KEY to connect managed apps, or use bundled apps (Linear, Notion, n8n) and Connect MCP.",
        );
    };

    let (display_name, _, _) = catalog_metadata_for_slug(slug);
    let capability = random_base64url(32);
    let transaction_id = random_base64url(32);
    let challenge = random_base64url(32);
    let bridge_key = random_base64url(32);
    let disc_id = uuid();
    let connect_url = format!("{}/api/apps/connect/{capability}", ctx.live.origin());
    let state_hash = sha256_hex(transaction_id.as_bytes());
    let link_hash = sha256_hex(connect_url.as_bytes());
    let expires_at = iso(crate::util::now() + ChronoDuration::minutes(10));
    let record = json!({
        "id": disc_id,
        "service": slug,
        "name": display_name,
        "kind": "composio_proxy",
        "proxyOrigin": proxy_origin,
        "challenge": challenge,
        "oauthStatus": "pending",
        "oauthStateHash": state_hash,
        "linkHash": link_hash,
        "callbackReturnTo": return_to,
        "expiresAt": expires_at,
    });
    if let Err(err) = ctx.store.put("app_discoveries", &disc_id, &record) {
        return Reply::error(500, err);
    }

    if let Ok(mut attempts) = proxy_attempts().lock() {
        attempts.retain(|_, a| a.expires_at > Instant::now());
        attempts.insert(
            capability.clone(),
            ProxyConnectAttempt {
                capability,
                transaction_id: transaction_id.clone(),
                discovery_id: disc_id,
                slug: slug.to_string(),
                challenge,
                bridge_key,
                origin: ctx.live.origin().to_string(),
                expires_at: Instant::now() + Duration::from_secs(600),
                bootstrapped: false,
                starting: false,
            },
        );
    }

    Reply::ok(json!({
        "url": connect_url,
        "attempt": transaction_id,
    }))
}

fn is_b64url_43(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn catalog_metadata_for_slug(slug: &str) -> (String, Option<String>, Vec<String>) {
    if let Ok(guard) = composio_cache().lock()
        && let Some(cached) = guard.as_ref()
        && let Some(item) = cached.items.iter().find(|i| text(i, "slug") == slug)
    {
        let name = text(item, "name");
        let logo = item.get("logoUrl").and_then(Value::as_str).map(str::to_string);
        let cats = item
            .get("categories")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        if !name.is_empty() {
            return (name, logo, cats);
        }
    }
    (slug.replace('_', " "), None, Vec::new())
}

fn loopback_endpoint_spec(origin: &str) -> Option<Value> {
    let parsed = parse_url(origin).ok()?;
    let host_kind = match parsed.host.as_str() {
        "localhost" => "localhost",
        "127.0.0.1" => "ipv4",
        "[::1]" => "ipv6",
        _ => return None,
    };
    let port = parsed
        .port
        .unwrap_or(if parsed.scheme == "https" { 443 } else { 80 });
    Some(json!({
        "type": "loopback",
        "host": host_kind,
        "port": port,
    }))
}

fn render_proxy_connect_html(
    broker_origin: &str,
    id: &str,
    expires_at_ms: u64,
    bridge_key: &str,
    nonce: &str,
) -> String {
    let values = json!({
        "brokerOrigin": broker_origin,
        "id": id,
        "expiresAt": expires_at_ms,
        "bridgeKey": bridge_key,
    })
    .to_string()
    .replace('<', "\\u003c");
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Connect your app · Nod</title><style nonce="{nonce}">:root{{color-scheme:light dark}}body{{font:16px system-ui;margin:0;min-height:100svh;display:grid;place-items:center;background:light-dark(#fafafa,#191919);color:light-dark(#191919,#fafafa)}}main{{max-width:360px;padding:32px;text-align:center}}h1{{font-size:21px;font-weight:550;letter-spacing:-.4px}}p{{line-height:1.6;color:light-dark(#676767,#bfbfbf)}}button{{font:inherit;padding:12px 20px;border:0;border-radius:8px;background:light-dark(#191919,#fafafa);color:light-dark(#fafafa,#191919);cursor:pointer}}button:disabled{{opacity:.5;cursor:default}}a{{color:inherit;text-underline-offset:3px}}</style></head><body><main><h1>Connect your app</h1>{COMPOSIO_PRIVACY_NOTICE}<p id="status" role="status">Review the data path, then continue to the service’s permission screen.</p><button id="connect" type="button">Continue to sign in</button></main><script nonce="{nonce}">
const {{brokerOrigin,id,expiresAt,bridgeKey}}={values};
const button=document.getElementById('connect');
const status=document.getElementById('status');
let popup,key,started=false,signed=false,deadline=expiresAt;
const valid=()=>{{if(Date.now()>=deadline)throw new Error();}};
const fail=(message)=>{{button.disabled=true;status.textContent=message||'Sign-in could not be completed. Return to Nod and try again.';popup?.close();}};
const encoded=(bytes)=>btoa(String.fromCharCode(...bytes)).replaceAll('+','-').replaceAll('/','_').replaceAll('=','');
window.addEventListener('message',async(event)=>{{
if(!key||signed||event.origin!==brokerOrigin||event.source!==popup)return;
const value=event.data;
if(!value||value.type!=='nod:proxy-connect:challenge'||value.id!==id||typeof value.challenge!=='string'||value.challenge.length>512)return;
const parts=value.challenge.split('\n');
if(parts.length!==5||parts[0]!=='nod-proxy-connect-v2'||parts[1]!==id||parts[2]!==location.origin||!/^[a-f0-9]{{64}}$/.test(parts[3])||!/^[A-Za-z0-9_-]{{43}}$/.test(parts[4]))return;
signed=true;
try{{valid();const signature=new Uint8Array(await crypto.subtle.sign({{name:'ECDSA',hash:'SHA-256'}},key,new TextEncoder().encode(value.challenge)));valid();if(signature.length!==64)throw new Error();key=undefined;popup.postMessage({{type:'nod:proxy-connect:proof',id,signature:encoded(signature)}},brokerOrigin);status.textContent='Finish signing in in the other window. You can then return to Nod.';}}catch{{fail();}}
}});
button.addEventListener('click',async()=>{{
if(started)return;
try{{valid();popup=window.open('about:blank','_blank','popup,width=520,height=720');if(!popup){{status.textContent='Allow popups for Nod, then select Continue to sign in.';return;}}started=true;button.disabled=true;
const pair=await crypto.subtle.generateKey({{name:'ECDSA',namedCurve:'P-256'}},false,['sign','verify']);valid();key=pair.privateKey;
const response=await fetch(location.pathname,{{method:'POST',credentials:'same-origin',headers:{{'X-Nod-Bridge':bridgeKey,'Content-Type':'application/json'}},body:JSON.stringify({{browserKey:await crypto.subtle.exportKey('jwk',pair.publicKey)}})}});if(!response.ok){{const reply=await response.json().catch(()=>({{}}));throw Object.assign(new Error(),{{reply:typeof reply?.error==='string'?reply.error.slice(0,300):''}});}}
const attempt=await response.json();const target=new URL(attempt.url);if(attempt.id!==id||!Number.isSafeInteger(attempt.expiresAt)||target.origin!==brokerOrigin||target.pathname!=='/v2/connect/pair'||target.searchParams.size!==1||!target.searchParams.get('ticket')||target.username||target.password||target.hash)throw new Error();deadline=Math.min(deadline,attempt.expiresAt);valid();if(popup.closed)throw new Error();popup.location.replace(target.href);status.textContent='Finish signing in in the other window.';
}}catch(error){{key=undefined;fail(error?.reply);}}
}});
setTimeout(()=>{{key=undefined;button.disabled=true;if(!signed)fail();}},Math.max(0,expiresAt-Date.now()));
</script></body></html>"#
    )
}

fn proxy_connect_bridge(ctx: &Ctx, capability: &str) -> Reply {
    if !is_b64url_43(capability) {
        return Reply::error(403, "This app connection is unavailable.");
    }
    let Some(proxy_origin) = composio_proxy_origin() else {
        return Reply::error(403, "This app connection is unavailable.");
    };
    let nonce = random_base64url(24);
    let csp = format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );

    if ctx.method == "GET" {
        let mut snapshot: Option<(String, String, u64)> = None;
        if let Ok(mut guard) = proxy_attempts().lock()
            && let Some(attempt) = guard.get_mut(capability)
            && attempt.expires_at > Instant::now()
            && !attempt.starting
        {
            attempt.bootstrapped = true;
            let remaining_ms = attempt
                .expires_at
                .saturating_duration_since(Instant::now())
                .as_millis() as u64;
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            snapshot = Some((
                attempt.transaction_id.clone(),
                attempt.bridge_key.clone(),
                now_ms.saturating_add(remaining_ms),
            ));
        }
        let Some((tx_id, bridge_key, exp_ms)) = snapshot else {
            return Reply::error(403, "This app connection is unavailable.");
        };
        let html = render_proxy_connect_html(&proxy_origin, &tx_id, exp_ms, &bridge_key, &nonce);
        return Reply::text(200, html, "text/html; charset=utf-8")
            .with_header("Referrer-Policy", "no-referrer")
            .with_header("Content-Security-Policy", csp);
    }

    if ctx.method != "POST" {
        return Reply::error(405, "Method not allowed.");
    }

    let header_bridge = ctx.query.get("__x_nod_bridge").map(String::as_str).unwrap_or("");
    let req_origin = ctx.query.get("__req_origin").map(String::as_str).unwrap_or("");
    let attempt_info = {
        let Ok(mut guard) = proxy_attempts().lock() else {
            return Reply::error(500, "Connection state unavailable.");
        };
        let Some(attempt) = guard.get_mut(capability) else {
            return Reply::error(403, "This app connection is unavailable.");
        };
        if !attempt.bootstrapped
            || attempt.starting
            || attempt.expires_at <= Instant::now()
            || header_bridge != attempt.bridge_key
            || (!req_origin.is_empty() && req_origin != attempt.origin)
        {
            return Reply::error(403, "This app connection is unavailable.");
        }
        attempt.starting = true;
        attempt.clone()
    };

    let Some(bk) = ctx.body.get("browserKey").and_then(Value::as_object) else {
        return Reply::error(400, "Invalid browser connection key.");
    };
    let kty = bk.get("kty").and_then(Value::as_str).unwrap_or("");
    let crv = bk.get("crv").and_then(Value::as_str).unwrap_or("");
    let x = bk.get("x").and_then(Value::as_str).unwrap_or("");
    let y = bk.get("y").and_then(Value::as_str).unwrap_or("");
    if kty != "EC" || crv != "P-256" || !is_b64url_43(x) || !is_b64url_43(y) {
        return Reply::error(400, "Invalid browser connection key.");
    }

    let endpoint_spec = loopback_endpoint_spec(&attempt_info.origin).unwrap_or_else(|| {
        json!({
            "type": "https",
            "url": format!("{}/api/apps/callback", attempt_info.origin),
        })
    });
    let ui_origin_spec = loopback_endpoint_spec(&attempt_info.origin).unwrap_or_else(|| {
        json!({
            "type": "https",
            "url": attempt_info.origin,
        })
    });

    let start_payload = json!({
        "toolkit": attempt_info.slug,
        "callback": endpoint_spec,
        "transactionId": attempt_info.transaction_id,
        "challenge": attempt_info.challenge,
        "browserKey": { "kty": "EC", "crv": "P-256", "x": x, "y": y },
        "uiOrigin": ui_origin_spec,
    });

    let result = match proxy_signed_request(
        ctx.store,
        &proxy_origin,
        "POST",
        "/v2/connect/start",
        Some(&start_payload),
    ) {
        Ok(val) => val,
        Err(err) => {
            if let Ok(mut guard) = proxy_attempts().lock() {
                guard.remove(&attempt_info.capability);
            }
            ctx.store.delete("app_discoveries", &attempt_info.discovery_id);
            return Reply::error(400, err);
        }
    };

    let redirect_url = text(&result, "redirectUrl");
    let ret_tx = text(&result, "transactionId");
    if ret_tx != attempt_info.transaction_id
        || !redirect_url.starts_with(&format!("{proxy_origin}/v2/connect/pair?ticket="))
    {
        if let Ok(mut guard) = proxy_attempts().lock() {
            guard.remove(&attempt_info.capability);
        }
        return Reply::error(400, "The app sign-in address could not be verified.");
    }

    let remaining_ms = attempt_info
        .expires_at
        .saturating_duration_since(Instant::now())
        .as_millis() as u64;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    Reply::ok(json!({
        "id": attempt_info.transaction_id,
        "url": redirect_url,
        "expiresAt": now_ms.saturating_add(remaining_ms),
    }))
}

fn start_composio_link(
    api_key: &str,
    slug: &str,
    callback_url: &str,
    owner_id: &str,
) -> Result<(String, String), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(6))
        .timeout(Duration::from_secs(12))
        .build();
    let auth_cfg_url = format!(
        "{COMPOSIO_API_BASE}/auth_configs?toolkit_slug={}",
        url_encode_component(slug)
    );
    let cfg_resp = agent
        .get(&auth_cfg_url)
        .set("x-api-key", api_key)
        .set("Accept", "application/json")
        .call()
        .map_err(|e| format!("Could not query Composio auth configs for {slug}: {e}"))?;
    let cfg_json: Value = serde_json::from_str(&cfg_resp.into_string().unwrap_or_default())
        .map_err(|_| "Invalid Composio auth configs response.".to_string())?;
    let auth_config_id = cfg_json
        .get("items")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|c| c.get("is_disabled").and_then(Value::as_bool) != Some(true))
                .and_then(|c| c.get("id").and_then(Value::as_str))
        })
        .ok_or_else(|| format!("No enabled Composio auth configuration exists for {slug}."))?;

    let link_resp = agent
        .post(&format!("{COMPOSIO_API_BASE}/connected_accounts/link"))
        .set("x-api-key", api_key)
        .set("Content-Type", "application/json")
        .send_string(
            &json!({
                "auth_config_id": auth_config_id,
                "user_id": owner_id,
                "callback_url": callback_url,
            })
            .to_string(),
        )
        .map_err(|e| format!("Could not start Composio sign-in link: {e}"))?;
    let link_json: Value = serde_json::from_str(&link_resp.into_string().unwrap_or_default())
        .map_err(|_| "Invalid Composio link response.".to_string())?;
    let redirect_url = link_json
        .get("redirect_url")
        .or_else(|| link_json.get("redirectUrl"))
        .and_then(Value::as_str)
        .ok_or_else(|| "Composio did not return a sign-in link.".to_string())?
        .to_string();
    let conn_id = link_json
        .get("connected_account_id")
        .or_else(|| link_json.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok((redirect_url, conn_id))
}

fn create_mcp_discovery(ctx: &Ctx) -> Reply {
    let raw_name = match ctx.body.get("name").and_then(Value::as_str) {
        Some(n) => n.trim().to_string(),
        None => return Reply::error(400, "Enter a name of 1 to 80 characters."),
    };
    if !is_valid_app_name(&raw_name) {
        return Reply::error(400, "Enter a name of 1 to 80 characters.");
    }
    let raw_endpoint = match ctx.body.get("endpoint").and_then(Value::as_str) {
        Some(e) if !e.trim().is_empty() => {
            let t = e.trim();
            if t.contains("://") {
                t.to_string()
            } else {
                format!("https://{t}")
            }
        }
        _ => return Reply::error(400, "Enter a valid server URL, such as https://example.com/mcp."),
    };
    let auth = match ctx.body.get("auth").and_then(Value::as_str) {
        Some(a @ ("oauth" | "bearer" | "x-api-key")) => a.to_string(),
        _ => return Reply::error(400, "Enter a name, server URL and authentication method."),
    };
    let client_id_required = ctx
        .body
        .get("clientIdRequired")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if client_id_required && auth != "oauth" {
        return Reply::error(400, "Enter a name, server URL and authentication method.");
    }

    let (parsed, is_local) = match check_public_https_url(&raw_endpoint, false, true) {
        Ok(res) => res,
        Err(reason) => {
            return Reply::error(
                400,
                format!("Use a public HTTPS server address without a query or fragment. {reason}"),
            );
        }
    };

    let endpoint_href = parsed.normalized;
    let service = format!("mcp-{}", &sha256_hex(endpoint_href.as_bytes())[..24]);
    let id = uuid();
    let expires_at = iso(crate::util::now() + ChronoDuration::minutes(15));
    let mut path_obj = json!({
        "id": uuid(),
        "kind": "mcp",
        "endpoint": endpoint_href,
        "auth": auth,
        "local": is_local,
    });
    if client_id_required {
        path_obj["clientIdRequired"] = json!(true);
    }

    let discovery = json!({
        "id": id,
        "name": raw_name,
        "service": service,
        "catalogApp": false,
        "isMcp": true,
        "needsAddress": false,
        "oauthStatus": Value::Null,
        "oauthError": Value::Null,
        "callbackReturnTo": "apps",
        "paths": [path_obj],
        "expiresAt": expires_at,
    });
    if let Err(err) = ctx.store.put("app_discoveries", &id, &discovery) {
        return Reply::error(500, err);
    }
    Reply::ok(json!({ "id": id }))
}

fn get_discovery(ctx: &Ctx, id: &str) -> Reply {
    let Some(disc) = ctx.store.get("app_discoveries", id) else {
        return Reply::error(404, "App setup session not found.");
    };
    let grant = ctx.store.get("app_discovery_grants", id);
    let state_hash = text(&disc, "oauthStateHash");
    let attempt = grant
        .as_ref()
        .and_then(|g| g.get("attempt").and_then(Value::as_str))
        .filter(|a| !state_hash.is_empty() && sha256_hex(a.as_bytes()) == state_hash)
        .map(str::to_string);
    let authorization_url = grant
        .as_ref()
        .and_then(|g| g.get("url").and_then(Value::as_str))
        .filter(|_| attempt.is_some() || state_hash.is_empty())
        .map(str::to_string);

    let service = text(&disc, "service");
    let needs_address = disc
        .get("needsAddress")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && disc
            .get("paths")
            .and_then(Value::as_array)
            .map(|a| a.is_empty())
            .unwrap_or(true);
    let paths = disc.get("paths").cloned().unwrap_or_else(|| json!([]));
    let client_id_required = paths
        .as_array()
        .map(|arr| {
            arr.iter()
                .any(|p| p.get("clientIdRequired").and_then(Value::as_bool) == Some(true))
        })
        .unwrap_or(false);

    let mut reply = json!({
        "service": service,
        "name": text(&disc, "name"),
        "catalogApp": disc.get("catalogApp").and_then(Value::as_bool).unwrap_or(false),
        "hints": disc.get("hints").cloned().unwrap_or_else(|| json!({})),
        "needsAddress": needs_address,
        "localAddress": disc.get("localAddress").cloned().unwrap_or(Value::Null),
        "oauthStatus": disc.get("oauthStatus").cloned().unwrap_or(Value::Null),
        "oauthError": disc.get("oauthError").cloned().unwrap_or(Value::Null),
        "authorizationUrl": authorization_url,
        "attempt": attempt,
        "paths": paths,
    });
    if client_id_required {
        reply["callbackUrl"] = json!(format!("{}/api/apps/callback", ctx.live.origin()));
    }
    Reply::ok(reply)
}

fn set_discovery_instance(ctx: &Ctx, id: &str) -> Reply {
    let Some(mut disc) = ctx.store.get("app_discoveries", id) else {
        return Reply::error(404, "App setup session expired. Start again.");
    };
    let raw_instance = text(&ctx.body, "instanceUrl").trim().to_string();
    if raw_instance.is_empty() {
        return Reply::error(400, "Enter your instance URL first.");
    }
    let normalized_base = if raw_instance.contains("://") {
        raw_instance
    } else {
        format!("https://{raw_instance}")
    };
    let (parsed, is_local) = match check_public_https_url(&normalized_base, false, true) {
        Ok(res) => res,
        Err(err) => return Reply::error(400, err),
    };
    let suffix = text(&disc, "instancePath");
    let suffix_clean = if suffix.is_empty() {
        "/mcp-server/http"
    } else {
        suffix.as_str()
    };
    let base_trimmed = parsed.origin.trim_end_matches('/');
    let endpoint = if parsed.path.ends_with(suffix_clean) {
        parsed.normalized
    } else {
        let base_path = parsed.path.trim_end_matches('/');
        format!("{base_trimmed}{base_path}{suffix_clean}")
    };

    let return_to = match text(&ctx.body, "returnTo").as_str() {
        "desktop" => "desktop",
        "window" => "window",
        _ => "apps",
    };

    let token = text(&ctx.body, "token").trim().to_string();
    let slug = text(&disc, "service");
    let name = text(&disc, "name");

    if !token.is_empty() {
        let tools = match verify_mcp_connection(&endpoint, "bearer", &token) {
            Ok(t) => t,
            Err(err) => return Reply::error(409, redact_secret(&err, &token)),
        };
        let path_obj = json!({
            "id": uuid(),
            "kind": "mcp",
            "endpoint": endpoint,
            "auth": "bearer",
            "local": is_local,
        });
        let account_label = parsed.host;
        match save_mcp_connection(
            ctx,
            &slug,
            &name,
            &account_label,
            &path_obj,
            &token,
            &tools,
            false,
        ) {
            Ok(saved) => {
                ctx.store.delete("app_discoveries", id);
                ctx.store.delete("app_discovery_grants", id);
                return Reply::ok(saved);
            }
            Err(err) => return Reply::error(500, err),
        }
    }

    let path_obj = json!({
        "id": uuid(),
        "kind": "mcp",
        "endpoint": endpoint,
        "auth": "oauth",
        "local": is_local,
    });
    if let Some(obj) = disc.as_object_mut() {
        obj.insert("needsAddress".to_string(), json!(false));
        obj.insert("callbackReturnTo".to_string(), json!(return_to));
        obj.insert("paths".to_string(), json!([path_obj]));
    }
    if let Err(err) = ctx.store.put("app_discoveries", id, &disc) {
        return Reply::error(500, err);
    }

    match start_mcp_oauth(ctx, id, &disc, None, Some(return_to)) {
        Ok(res) => Reply::ok(res),
        Err(err) => Reply::error(409, format!("{err} If this instance uses an access token, expand \"Use an access token instead\".")),
    }
}

fn authorize_discovery(ctx: &Ctx, id: &str) -> Reply {
    let Some(disc) = ctx.store.get("app_discoveries", id) else {
        return Reply::error(404, "App setup session expired. Start again.");
    };
    let client_id = ctx
        .body
        .get("clientId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let return_to = match text(&ctx.body, "returnTo").as_str() {
        "onboarding" => Some("onboarding"),
        "window" => Some("window"),
        "desktop" => Some("desktop"),
        "apps" => Some("apps"),
        _ => None,
    };
    match start_mcp_oauth(ctx, id, &disc, client_id, return_to) {
        Ok(res) => Reply::ok(res),
        Err(err) => Reply::error(502, err),
    }
}

fn discover_oauth_metadata(endpoint: &str, allow_local: bool) -> Result<Value, String> {
    let parsed = parse_url(endpoint)?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(6))
        .timeout(Duration::from_secs(10))
        .build();

    let mut auth_server_origin = parsed.origin.clone();
    let prm_path_specific = format!(
        "{}/.well-known/oauth-protected-resource{}",
        parsed.origin,
        if parsed.path == "/" { "" } else { parsed.path.as_str() }
    );
    let prm_root = format!("{}/.well-known/oauth-protected-resource", parsed.origin);

    for candidate in [&prm_path_specific, &prm_root] {
        if check_public_https_url(candidate, false, allow_local).is_err() {
            continue;
        }
        if let Ok(resp) = agent.get(candidate).set("Accept", "application/json").call()
            && let Ok(body) = resp.into_string()
            && let Ok(json_val) = serde_json::from_str::<Value>(&body)
            && let Some(first_as) = json_val
                .pointer("/authorization_servers/0")
                .and_then(Value::as_str)
        {
            auth_server_origin = first_as.trim_end_matches('/').to_string();
            break;
        }
    }

    let as_urls = [
        format!("{auth_server_origin}/.well-known/oauth-authorization-server"),
        format!("{auth_server_origin}/.well-known/openid-configuration"),
    ];
    for url in &as_urls {
        check_public_https_url(url, false, allow_local)?;
        if let Ok(resp) = agent.get(url).set("Accept", "application/json").call()
            && let Ok(body) = resp.into_string()
            && let Ok(meta) = serde_json::from_str::<Value>(&body)
            && meta.get("authorization_endpoint").and_then(Value::as_str).is_some()
            && meta.get("token_endpoint").and_then(Value::as_str).is_some()
        {
            return Ok(meta);
        }
    }

    Err(format!(
        "The server at {} does not advertise OAuth 2.0 discovery metadata.",
        parsed.origin
    ))
}

fn dynamic_client_register(
    registration_endpoint: &str,
    callback_url: &str,
    allow_local: bool,
) -> Result<String, String> {
    check_public_https_url(registration_endpoint, true, allow_local)?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(6))
        .timeout(Duration::from_secs(10))
        .build();
    let body = json!({
        "client_name": "Nod",
        "redirect_uris": [callback_url],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none"
    });
    let resp = agent
        .post(registration_endpoint)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| format!("OAuth client registration failed: {e}"))?;
    let txt = resp
        .into_string()
        .map_err(|e| format!("Invalid client registration response: {e}"))?;
    let parsed: Value =
        serde_json::from_str(&txt).map_err(|_| "Invalid JSON from client registration.".to_string())?;
    parsed
        .get("client_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "OAuth registration response missing client_id.".to_string())
}

fn start_mcp_oauth(
    ctx: &Ctx,
    disc_id: &str,
    disc: &Value,
    manual_client_id: Option<String>,
    return_to: Option<&str>,
) -> Result<Value, String> {
    let path_obj = disc
        .pointer("/paths/0")
        .ok_or_else(|| "Missing connection path.".to_string())?;
    let endpoint = text(path_obj, "endpoint");
    let allow_local = path_obj.get("local").and_then(Value::as_bool).unwrap_or(false)
        || is_loopback_hostname(
            &parse_url(&endpoint).map(|p| p.host).unwrap_or_default(),
        );
    check_public_https_url(&endpoint, false, allow_local)?;

    let meta = discover_oauth_metadata(&endpoint, allow_local)?;
    let auth_endpoint = meta
        .get("authorization_endpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| "Missing authorization_endpoint.".to_string())?;
    let token_endpoint = meta
        .get("token_endpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| "Missing token_endpoint.".to_string())?;
    check_public_https_url(auth_endpoint, true, allow_local)?;
    check_public_https_url(token_endpoint, true, allow_local)?;

    let callback_url = format!("{}/api/apps/callback", ctx.live.origin());
    let client_id = if let Some(cid) = manual_client_id {
        cid
    } else if let Some(reg_ep) = meta.get("registration_endpoint").and_then(Value::as_str) {
        dynamic_client_register(reg_ep, &callback_url, allow_local)?
    } else {
        let mut updated = disc.clone();
        if let Some(arr) = updated.get_mut("paths").and_then(Value::as_array_mut)
            && let Some(first) = arr.first_mut().and_then(Value::as_object_mut)
        {
            first.insert("clientIdRequired".to_string(), json!(true));
        }
        let _ = ctx.store.put("app_discoveries", disc_id, &updated);
        return Err("This OAuth server requires a Client ID.".to_string());
    };

    let state = random_base64url(32);
    let verifier = random_base64url(32);
    let challenge = {
        let mut hasher = sha2::Sha256::new();
        use sha2::Digest;
        hasher.update(verifier.as_bytes());
        base64url_encode(&hasher.finalize())
    };
    let scope = path_obj.get("scope").and_then(Value::as_str);

    let sep = if auth_endpoint.contains('?') { '&' } else { '?' };
    let mut auth_url = format!(
        "{auth_endpoint}{sep}response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        url_encode_component(&client_id),
        url_encode_component(&callback_url),
        url_encode_component(&challenge),
        url_encode_component(&state)
    );
    if let Some(sc) = scope
        && !sc.is_empty()
    {
        auth_url.push_str("&scope=");
        auth_url.push_str(&url_encode_component(sc));
    }

    let state_hash = sha256_hex(state.as_bytes());
    let link_hash = sha256_hex(auth_url.as_bytes());
    let mut updated = disc.clone();
    if let Some(obj) = updated.as_object_mut() {
        obj.insert("oauthStatus".to_string(), json!("pending"));
        obj.insert("oauthError".to_string(), Value::Null);
        obj.insert("oauthStateHash".to_string(), json!(state_hash));
        obj.insert("linkHash".to_string(), json!(link_hash));
        if let Some(rt) = return_to {
            obj.insert("callbackReturnTo".to_string(), json!(rt));
        }
    }
    ctx.store
        .put("app_discoveries", disc_id, &updated)
        .map_err(|e| format!("Failed saving discovery state: {e}"))?;

    let grant_plain = json!({
        "verifier": verifier,
        "clientId": client_id,
        "callbackUrl": callback_url,
        "tokenEndpoint": token_endpoint,
        "issuer": meta.get("issuer").cloned().unwrap_or(Value::Null),
        "allowLocal": allow_local,
    })
    .to_string();
    let grant_enc = vault::encrypt_secret(&grant_plain)?;
    ctx.store
        .put(
            "app_discovery_grants",
            disc_id,
            &json!({
                "id": disc_id,
                "url": auth_url,
                "attempt": state,
                "grantEnc": grant_enc,
            }),
        )
        .map_err(|e| format!("Failed storing grant: {e}"))?;

    Ok(json!({
        "url": auth_url,
        "attempt": state,
    }))
}

fn parse_mcp_json_rpc_response(body: &str) -> Result<Value, String> {
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed)
            .map_err(|e| format!("Invalid MCP JSON response: {e}"));
    }
    let mut last_json: Option<Value> = None;
    for line in trimmed.lines() {
        let line = line.trim();
        if let Some(data) = line.strip_prefix("data:") {
            let payload = data.trim();
            if let Ok(val) = serde_json::from_str::<Value>(payload) {
                last_json = Some(val);
            }
        }
    }
    last_json.ok_or_else(|| "Server did not return a valid MCP JSON-RPC response.".to_string())
}

fn mcp_post(
    agent: &ureq::Agent,
    endpoint: &str,
    auth: &str,
    token: &str,
    session_id: Option<&str>,
    payload: &Value,
) -> Result<(Option<String>, Value), String> {
    let mut req = agent
        .post(endpoint)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json, text/event-stream")
        .set("User-Agent", "nod/0.1.0");
    if !token.is_empty() {
        if auth == "x-api-key" {
            req = req.set("x-api-key", token);
        } else {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
    }
    if let Some(sid) = session_id {
        req = req.set("mcp-session-id", sid);
    }
    match req.send_string(&payload.to_string()) {
        Ok(resp) => {
            let next_sid = resp
                .header("mcp-session-id")
                .map(str::to_string)
                .or_else(|| session_id.map(str::to_string));
            let text_body = resp
                .into_string()
                .map_err(|e| format!("Failed reading MCP response: {e}"))?;
            if text_body.trim().is_empty() {
                return Ok((next_sid, json!({})));
            }
            let val = parse_mcp_json_rpc_response(&text_body)?;
            if let Some(err) = val.get("error") {
                return Err(format!("MCP error: {err}"));
            }
            Ok((next_sid, val))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp.into_string().unwrap_or_default();
            let snippet: String = detail.chars().take(400).collect();
            Err(format!("MCP server answered HTTP {code}: {snippet}"))
        }
        Err(ureq::Error::Transport(t)) => Err(format!("Could not reach MCP server: {t}")),
    }
}

fn verify_mcp_connection(endpoint: &str, auth: &str, token: &str) -> Result<Vec<Value>, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(6))
        .timeout(Duration::from_secs(15))
        .build();
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "nod", "version": "0.1.0" }
        }
    });
    let (session_id, _) = mcp_post(&agent, endpoint, auth, token, None, &init_req)?;

    let notify_init = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let _ = mcp_post(
        &agent,
        endpoint,
        auth,
        token,
        session_id.as_deref(),
        &notify_init,
    );

    let list_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    let (_, list_resp) = mcp_post(&agent, endpoint, auth, token, session_id.as_deref(), &list_req)?;
    let tools = list_resp
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if tools.is_empty() {
        return Err("The MCP server returned no tools.".to_string());
    }
    Ok(tools
        .into_iter()
        .take(200)
        .map(|t| {
            json!({
                "name": text(&t, "name"),
                "description": text(&t, "description"),
                "inputSchema": t.get("inputSchema").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            })
        })
        .collect())
}

fn redact_secret(text_in: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text_in.to_string();
    }
    text_in.replace(secret, "[redacted]")
}

#[allow(clippy::too_many_arguments)]
fn save_mcp_connection(
    ctx: &Ctx,
    slug: &str,
    name: &str,
    account_name: &str,
    path_obj: &Value,
    secret_payload: &str,
    tools: &[Value],
    is_manual_mcp: bool,
) -> Result<Value, String> {
    let enc_token = vault::encrypt_secret(secret_payload)?;
    let connection_id = uuid();
    let logo_url = if is_manual_mcp {
        "/logos/server.svg".to_string()
    } else if let Some(b) = bundled_app(slug) {
        format!("/api/apps/catalog/{}/icon", b.slug)
    } else {
        format!("/logos/{slug}.svg")
    };
    let tool_names: Vec<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    let bundled_meta = bundled_app(slug);
    let description = if let Some(b) = bundled_meta {
        b.description.clone()
    } else {
        format!(
            "MCP server ({} tools): {}",
            tool_names.len(),
            tool_names.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
        )
    };
    let categories = bundled_meta
        .map(|b| Value::Array(b.categories.iter().map(|c| json!(c)).collect()))
        .unwrap_or_else(|| json!(["MCP"]));

    let record = json!({
        "id": connection_id,
        "connectionId": connection_id,
        "slug": slug,
        "name": name,
        "description": description,
        "logoUrl": logo_url,
        "categories": categories,
        "status": "connected",
        "direct": true,
        "canRename": is_manual_mcp,
        "canReconnect": true,
        "accountName": account_name,
        "triggersEnabled": false,
        "supportsTriggers": false,
        "accessPaths": [path_obj],
        "tools": tool_names,
        "toolSchemas": tools,
        "tokenEnc": enc_token,
        "connectedAt": iso_now(),
    });
    ctx.store.put("apps", &connection_id, &record)?;
    notify_apps(ctx);
    Ok(public_connection(&record))
}

fn complete_discovery(ctx: &Ctx, id: &str) -> Reply {
    let Some(disc) = ctx.store.get("app_discoveries", id) else {
        return Reply::error(404, "App setup session expired. Start again.");
    };
    let slug = text(&disc, "service");
    let name = text(&disc, "name");
    let is_manual_mcp = disc.get("isMcp").and_then(Value::as_bool).unwrap_or(false);
    let path_obj = disc.pointer("/paths/0").cloned().unwrap_or(Value::Null);
    let endpoint = text(&path_obj, "endpoint");
    let auth = text(&path_obj, "auth");
    if auth == "oauth" {
        return Reply::error(400, "Complete OAuth sign-in in your browser.");
    }

    let account_name = match ctx.body.get("accountName").and_then(Value::as_str) {
        Some(a) if !a.trim().is_empty() && a.trim().chars().count() <= 200 => a.trim().to_string(),
        _ => return Reply::error(400, "Enter an account name."),
    };
    let token_raw = match ctx.body.get("token") {
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    };
    if token_raw.is_empty() {
        return Reply::error(400, "Enter an access token or API key.");
    }

    let tools = match verify_mcp_connection(&endpoint, &auth, &token_raw) {
        Ok(t) => t,
        Err(err) => {
            return Reply::error(
                409,
                format!("The connection could not be verified: {}", redact_secret(&err, &token_raw)),
            );
        }
    };

    match save_mcp_connection(
        ctx,
        &slug,
        &name,
        &account_name,
        &path_obj,
        &token_raw,
        &tools,
        is_manual_mcp,
    ) {
        Ok(saved) => {
            ctx.store.delete("app_discoveries", id);
            ctx.store.delete("app_discovery_grants", id);
            Reply::ok(saved)
        }
        Err(err) => Reply::error(500, err),
    }
}

fn oauth_callback(ctx: &Ctx) -> Reply {
    let state = ctx.param("state").unwrap_or_default();
    let code = ctx.param("code").unwrap_or_default();
    let completion_token = ctx.param("completion_token").unwrap_or_default();
    let oauth_err = ctx.param("error").unwrap_or_default();
    let iss = ctx.param("iss").unwrap_or_default();
    let state_hash = sha256_hex(state.as_bytes());

    let Some(mut disc) = ctx
        .store
        .all("app_discoveries")
        .into_iter()
        .find(|d| text(d, "oauthStateHash") == state_hash && text(d, "oauthStatus") == "pending")
    else {
        return Reply::redirect("/connected.html?connection=expired&desktop=1");
    };
    let expired = parse_iso(&text(&disc, "expiresAt"))
        .map(|exp| exp <= crate::util::now())
        .unwrap_or(false);
    if expired {
        return Reply::redirect("/connected.html?connection=expired&desktop=1");
    }

    let disc_id = text(&disc, "id");
    let slug = text(&disc, "service");
    let name = text(&disc, "name");
    let is_desktop = text(&disc, "callbackReturnTo") == "desktop";
    let desktop_q = if is_desktop { "&desktop=1" } else { "" };

    if text(&disc, "kind") == "composio_proxy" {
        if !oauth_err.is_empty() || completion_token.is_empty() {
            let outcome = if oauth_err == "access_denied" {
                "cancelled"
            } else {
                "failed"
            };
            if let Some(obj) = disc.as_object_mut() {
                obj.insert("oauthStatus".to_string(), json!(outcome));
                obj.insert(
                    "oauthError".to_string(),
                    if outcome == "cancelled" {
                        Value::Null
                    } else {
                        json!("Sign-in did not finish.")
                    },
                );
                obj.insert("oauthStateHash".to_string(), Value::Null);
            }
            let _ = ctx.store.put("app_discoveries", &disc_id, &disc);
            return Reply::redirect(format!(
                "/connected.html?connection={outcome}&service={}&serviceName={}&attempt={}{desktop_q}",
                url_encode_component(&slug),
                url_encode_component(&name),
                url_encode_component(&state),
            ));
        }

        let proxy_origin = text(&disc, "proxyOrigin");
        let challenge = text(&disc, "challenge");
        let complete_res = proxy_signed_request(
            ctx.store,
            &proxy_origin,
            "POST",
            "/v2/connect/complete",
            Some(&json!({
                "completionToken": completion_token,
                "transactionId": state,
                "challenge": challenge,
            })),
        );
        return match complete_res {
            Ok(done)
                if text(&done, "toolkit") == slug
                    && text(&done, "status") == "ACTIVE"
                    && !text(&done, "connectedAccountId").is_empty() =>
            {
                let conn_id = text(&done, "connectedAccountId");
                let (cat_name, logo_opt, categories) = catalog_metadata_for_slug(&slug);
                let final_name = if name.is_empty() { cat_name } else { name };
                let mut record = json!({
                    "id": uuid(),
                    "connectionId": conn_id,
                    "slug": slug,
                    "name": final_name,
                    "kind": "composio_proxy",
                    "proxyOrigin": proxy_origin,
                    "description": "",
                    "categories": categories,
                    "status": "connected",
                    "triggersEnabled": false,
                    "supportsTriggers": false,
                    "accountName": final_name,
                    "connectedAt": iso_now(),
                });
                if let Some(logo) = logo_opt {
                    record["logoUrl"] = json!(logo);
                }
                let _ = ctx.store.put("apps", &text(&record, "id"), &record);
                if let Some(obj) = disc.as_object_mut() {
                    obj.insert("oauthStatus".to_string(), json!("connected"));
                    obj.insert("oauthStateHash".to_string(), Value::Null);
                }
                let _ = ctx.store.put("app_discoveries", &disc_id, &disc);
                if let Ok(mut attempts) = proxy_attempts().lock() {
                    attempts.retain(|_, a| a.discovery_id != disc_id);
                }
                notify_apps(ctx);
                Reply::redirect(format!(
                    "/connected.html?connection=connected&service={}&serviceName={}&attempt={}{desktop_q}",
                    url_encode_component(&slug),
                    url_encode_component(&final_name),
                    url_encode_component(&state),
                ))
            }
            Ok(_) | Err(_) => {
                if let Some(obj) = disc.as_object_mut() {
                    obj.insert("oauthStatus".to_string(), json!("failed"));
                    obj.insert("oauthError".to_string(), json!("The connected app could not be verified."));
                    obj.insert("oauthStateHash".to_string(), Value::Null);
                }
                let _ = ctx.store.put("app_discoveries", &disc_id, &disc);
                Reply::redirect(format!(
                    "/connected.html?connection=failed&service={}&serviceName={}&attempt={}{desktop_q}",
                    url_encode_component(&slug),
                    url_encode_component(&name),
                    url_encode_component(&state),
                ))
            }
        };
    }

    if !oauth_err.is_empty() || code.is_empty() {
        let outcome = if oauth_err == "access_denied" {
            "cancelled"
        } else {
            "failed"
        };
        if let Some(obj) = disc.as_object_mut() {
            obj.insert("oauthStatus".to_string(), json!(outcome));
            obj.insert(
                "oauthError".to_string(),
                if outcome == "cancelled" {
                    Value::Null
                } else {
                    json!(format!("Sign-in failed ({oauth_err})"))
                },
            );
            obj.insert("oauthStateHash".to_string(), Value::Null);
        }
        let _ = ctx.store.put("app_discoveries", &disc_id, &disc);
        ctx.store.delete("app_discovery_grants", &disc_id);
        return Reply::redirect(format!(
            "/connected.html?connection={outcome}&service={}&serviceName={}&attempt={}{desktop_q}",
            url_encode_component(&slug),
            url_encode_component(&name),
            url_encode_component(&state),
        ));
    }

    if text(&disc, "kind") == "composio" {
        let record = json!({
            "id": uuid(),
            "connectionId": text(&disc, "composioAccountId"),
            "slug": slug,
            "name": name,
            "description": "",
            "categories": [],
            "status": "connected",
            "triggersEnabled": false,
            "supportsTriggers": false,
            "accountName": name,
            "connectedAt": iso_now(),
        });
        let _ = ctx.store.put("apps", &text(&record, "id"), &record);
        if let Some(obj) = disc.as_object_mut() {
            obj.insert("oauthStatus".to_string(), json!("connected"));
            obj.insert("oauthStateHash".to_string(), Value::Null);
        }
        let _ = ctx.store.put("app_discoveries", &disc_id, &disc);
        notify_apps(ctx);
        return Reply::redirect(format!(
            "/connected.html?connection=connected&service={}&serviceName={}&attempt={}{desktop_q}",
            url_encode_component(&slug),
            url_encode_component(&name),
            url_encode_component(&state),
        ));
    }

    let finish = (|| -> Result<(), String> {
        let grant_doc = ctx
            .store
            .get("app_discovery_grants", &disc_id)
            .ok_or_else(|| "Sign-in state expired.".to_string())?;
        let grant_plain = vault::decrypt_secret(&text(&grant_doc, "grantEnc"))?;
        let grant: Value = serde_json::from_str(&grant_plain)
            .map_err(|_| "Corrupted sign-in state.".to_string())?;
        if !iss.is_empty()
            && let Some(expected_iss) = grant.get("issuer").and_then(Value::as_str)
            && !expected_iss.is_empty()
            && expected_iss != iss
        {
            return Err("The OAuth issuer does not match.".to_string());
        }
        let token_endpoint = text(&grant, "tokenEndpoint");
        let client_id = text(&grant, "clientId");
        let callback_url = text(&grant, "callbackUrl");
        let verifier = text(&grant, "verifier");
        let allow_local = grant.get("allowLocal").and_then(Value::as_bool).unwrap_or(false);
        check_public_https_url(&token_endpoint, true, allow_local)?;

        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(6))
            .timeout(Duration::from_secs(15))
            .build();
        let form_body = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
            url_encode_component(&code),
            url_encode_component(&callback_url),
            url_encode_component(&client_id),
            url_encode_component(&verifier)
        );
        let tok_resp = agent
            .post(&token_endpoint)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .set("Accept", "application/json")
            .send_string(&form_body)
            .map_err(|e| format!("Token exchange failed: {e}"))?;
        let tok_json: Value = serde_json::from_str(&tok_resp.into_string().unwrap_or_default())
            .map_err(|_| "Invalid OAuth token response.".to_string())?;
        let access_token = tok_json
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| "OAuth response missing access_token.".to_string())?
            .to_string();
        let refresh_token = tok_json
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string);
        let expires_at = tok_json
            .get("expires_in")
            .and_then(Value::as_i64)
            .map(|secs| iso(crate::util::now() + ChronoDuration::seconds(secs)));

        let path_obj = disc
            .pointer("/paths/0")
            .cloned()
            .ok_or_else(|| "Missing MCP connection path.".to_string())?;
        let endpoint = text(&path_obj, "endpoint");
        let tools = verify_mcp_connection(&endpoint, "bearer", &access_token)
            .map_err(|e| redact_secret(&e, &access_token))?;

        let secret_json = json!({
            "kind": "oauth",
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresAt": expires_at,
            "tokenEndpoint": token_endpoint,
            "clientId": client_id,
            "allowLocal": allow_local,
        })
        .to_string();
        let is_manual = disc.get("isMcp").and_then(Value::as_bool).unwrap_or(false);
        save_mcp_connection(ctx, &slug, &name, &name, &path_obj, &secret_json, &tools, is_manual)?;
        Ok(())
    })();

    match finish {
        Ok(()) => {
            if let Some(obj) = disc.as_object_mut() {
                obj.insert("oauthStatus".to_string(), json!("connected"));
                obj.insert("oauthStateHash".to_string(), Value::Null);
            }
            let _ = ctx.store.put("app_discoveries", &disc_id, &disc);
            ctx.store.delete("app_discovery_grants", &disc_id);
            Reply::redirect(format!(
                "/connected.html?connection=connected&service={}&serviceName={}&attempt={}{desktop_q}",
                url_encode_component(&slug),
                url_encode_component(&name),
                url_encode_component(&state),
            ))
        }
        Err(err) => {
            if let Some(obj) = disc.as_object_mut() {
                obj.insert("oauthStatus".to_string(), json!("failed"));
                obj.insert("oauthError".to_string(), json!(err.chars().take(500).collect::<String>()));
                obj.insert("oauthStateHash".to_string(), Value::Null);
            }
            let _ = ctx.store.put("app_discoveries", &disc_id, &disc);
            ctx.store.delete("app_discovery_grants", &disc_id);
            Reply::redirect(format!(
                "/connected.html?connection=failed&service={}&serviceName={}&attempt={}{desktop_q}",
                url_encode_component(&slug),
                url_encode_component(&name),
                url_encode_component(&state),
            ))
        }
    }
}

fn rename_mcp(ctx: &Ctx, service: &str) -> Reply {
    if !is_manual_mcp_service(service) || bundled_app(service).is_some() {
        return Reply::error(404, "This custom MCP server is unavailable.");
    }
    let Some(next_name) = ctx
        .body
        .get("name")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| is_valid_app_name(s))
    else {
        return Reply::error(400, "To rename this server, enter a name of 1 to 80 characters.");
    };

    let mut updated = 0;
    for mut app in ctx.store.all("apps") {
        if text(&app, "slug") == service {
            let id = text(&app, "id");
            if let Some(obj) = app.as_object_mut() {
                obj.insert("name".to_string(), json!(next_name));
            }
            if ctx.store.put("apps", &id, &app).is_ok() {
                updated += 1;
            }
        }
    }
    if updated == 0 {
        return Reply::error(404, "This custom MCP server is unavailable.");
    }
    notify_apps(ctx);
    Reply::ok(json!({ "name": next_name }))
}

fn disconnect_account(ctx: &Ctx, slug: &str, conn_id: &str) -> Reply {
    let Some(doc) = ctx.store.get("apps", conn_id) else {
        return Reply::error(404, "This connected account is unavailable.");
    };
    if text(&doc, "slug") != slug {
        return Reply::error(404, "This connected account is unavailable.");
    }
    ctx.store.delete("apps", conn_id);
    notify_apps(ctx);
    Reply::ok(json!({ "ok": true }))
}

fn toggle_triggers(ctx: &Ctx, slug: &str) -> Reply {
    let enabled = ctx
        .body
        .get("enabled")
        .or_else(|| ctx.body.get("triggersEnabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut updated = 0;
    for mut app in ctx.store.all("apps") {
        if text(&app, "slug") == slug {
            let id = text(&app, "id");
            if let Some(obj) = app.as_object_mut() {
                obj.insert("triggersEnabled".to_string(), json!(enabled));
            }
            if ctx.store.put("apps", &id, &app).is_ok() {
                updated += 1;
            }
        }
    }
    if updated == 0 {
        return Reply::error(404, "No connected account found for that app.");
    }
    notify_apps(ctx);
    Reply::ok(json!({ "ok": true, "triggersEnabled": enabled }))
}

fn speech(ctx: &Ctx) -> Reply {
    let prefs = preferences_state(ctx.store);
    let speech_voice = prefs.get("speechVoice").cloned().unwrap_or(Value::Null);
    let speech_reply = prefs
        .get("speechReply")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    Reply::ok(json!({
        "transcription": false,
        "synthesis": false,
        "voices": [],
        "speechVoice": speech_voice,
        "defaultVoice": "local:af_heart",
        "speechReply": speech_reply,
        "transcriptionFormat": "wav"
    }))
}

fn access_token_for_connection(store: &Store, conn: &Value) -> Result<(String, String), String> {
    let conn_id = text(conn, "id");
    let enc = text(conn, "tokenEnc");
    let raw_secret = vault::decrypt_secret(&enc)?;
    let path_auth = conn
        .pointer("/accessPaths/0/auth")
        .and_then(Value::as_str)
        .unwrap_or("bearer")
        .to_string();

    if path_auth != "oauth" {
        return Ok((path_auth, raw_secret));
    }

    let mut parsed: Value =
        serde_json::from_str(&raw_secret).map_err(|_| "Corrupted OAuth credential.".to_string())?;
    let access_token = text(&parsed, "accessToken");
    let expires_soon = parsed
        .get("expiresAt")
        .and_then(Value::as_str)
        .and_then(parse_iso)
        .map(|exp| exp <= crate::util::now() + ChronoDuration::seconds(60))
        .unwrap_or(false);
    if !expires_soon && !access_token.is_empty() {
        return Ok(("bearer".to_string(), access_token));
    }
    let refresh_token = text(&parsed, "refreshToken");
    let token_endpoint = text(&parsed, "tokenEndpoint");
    let client_id = text(&parsed, "clientId");
    let allow_local = parsed.get("allowLocal").and_then(Value::as_bool).unwrap_or(false);
    if refresh_token.is_empty() || token_endpoint.is_empty() {
        return Err("Reconnect this account in Apps to refresh its sign-in.".to_string());
    }
    check_public_https_url(&token_endpoint, true, allow_local)?;

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(6))
        .timeout(Duration::from_secs(15))
        .build();
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        url_encode_component(&refresh_token),
        url_encode_component(&client_id)
    );
    let resp = agent
        .post(&token_endpoint)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("Accept", "application/json")
        .send_string(&body)
        .map_err(|e| format!("Token refresh failed: {e}"))?;
    let tok_json: Value = serde_json::from_str(&resp.into_string().unwrap_or_default())
        .map_err(|_| "Invalid OAuth refresh response.".to_string())?;
    let next_access = tok_json
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| "OAuth refresh did not return an access_token.".to_string())?
        .to_string();
    if let Some(obj) = parsed.as_object_mut() {
        obj.insert("accessToken".to_string(), json!(next_access));
        if let Some(next_rt) = tok_json.get("refresh_token").and_then(Value::as_str) {
            obj.insert("refreshToken".to_string(), json!(next_rt));
        }
        if let Some(secs) = tok_json.get("expires_in").and_then(Value::as_i64) {
            obj.insert(
                "expiresAt".to_string(),
                json!(iso(crate::util::now() + ChronoDuration::seconds(secs))),
            );
        }
    }
    let next_enc = vault::encrypt_secret(&parsed.to_string())?;
    let mut updated_conn = conn.clone();
    if let Some(obj) = updated_conn.as_object_mut() {
        obj.insert("tokenEnc".to_string(), json!(next_enc));
    }
    let _ = store.put("apps", &conn_id, &updated_conn);
    Ok(("bearer".to_string(), next_access))
}

pub fn connected_mcp_tools(store: &Store) -> Vec<ConnectedMcpToolSpec> {
    let mut out = Vec::new();
    for app in store.all("apps") {
        if text(&app, "status") != "connected" {
            continue;
        }
        if app.pointer("/accessPaths/0/kind").and_then(Value::as_str) != Some("mcp") {
            continue;
        }
        let conn_id = text(&app, "id");
        let service = text(&app, "slug");
        let app_name = text(&app, "name");
        if let Some(schemas) = app.get("toolSchemas").and_then(Value::as_array) {
            for t in schemas {
                let tool_name = text(t, "name");
                if tool_name.is_empty() {
                    continue;
                }
                out.push(ConnectedMcpToolSpec {
                    connection_id: conn_id.clone(),
                    service: service.clone(),
                    app_name: app_name.clone(),
                    tool_name,
                    description: text(t, "description"),
                    input_schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                });
            }
        }
    }
    out
}

fn ensure_composio_proxy_session(
    store: &Store,
    connection_filter: Option<&str>,
) -> Result<(String, String), String> {
    let proxy_origin = composio_proxy_origin()
        .ok_or_else(|| "Composio proxy is not enabled in this environment.".to_string())?;
    let proxy_apps: Vec<Value> = store
        .all("apps")
        .into_iter()
        .filter(|a| text(a, "status") == "connected" && text(a, "kind") == "composio_proxy")
        .filter(|a| connection_filter.map(|cid| text(a, "id") == cid).unwrap_or(true))
        .collect();
    if proxy_apps.is_empty() {
        return Err("No Composio apps are connected.".to_string());
    }
    let identity = load_or_create_proxy_identity(store)?;
    let mut toolkits = Vec::new();
    let mut connected_accounts = serde_json::Map::new();
    for app in &proxy_apps {
        let slug = text(app, "slug");
        let cid = text(app, "connectionId");
        if !slug.is_empty() && !cid.is_empty() {
            if !toolkits.contains(&slug) {
                toolkits.push(slug.clone());
            }
            connected_accounts.entry(slug).or_insert_with(|| json!(cid));
        }
    }
    let cache_key = format!(
        "{}|{}|{}",
        proxy_origin,
        identity.user_id,
        Value::Object(connected_accounts.clone())
    );
    if let Ok(guard) = proxy_sessions().lock()
        && let Some((sid, created)) = guard.get(&cache_key)
        && created.elapsed() < Duration::from_secs(10 * 60)
    {
        return Ok((proxy_origin, sid.clone()));
    }

    let created = proxy_signed_request(
        store,
        &proxy_origin,
        "POST",
        "/v2/api/v3.1/tool_router/session",
        Some(&json!({
            "user_id": identity.user_id,
            "toolkits": toolkits,
            "connected_accounts": connected_accounts,
            "manage_connections": false,
        })),
    )?;
    let sid = created
        .get("session_id")
        .or_else(|| created.get("sessionId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Composio did not return a tool session ID.".to_string())?
        .to_string();
    if let Ok(mut guard) = proxy_sessions().lock() {
        guard.insert(cache_key, (sid.clone(), Instant::now()));
    }
    Ok((proxy_origin, sid))
}

pub fn call_connected_mcp_tool(
    store: &Store,
    connection_id: &str,
    tool_name: &str,
    input: Value,
) -> Result<Value, String> {
    let conn = store
        .get("apps", connection_id)
        .ok_or_else(|| "Connected app account not found.".to_string())?;
    if text(&conn, "kind") == "composio_proxy" {
        let account_id = text(&conn, "connectionId");
        let (proxy_origin, sid) = ensure_composio_proxy_session(store, Some(connection_id))?;
        let path = format!("/v2/api/v3.1/tool_router/session/{sid}/execute");
        let res = proxy_signed_request(
            store,
            &proxy_origin,
            "POST",
            &path,
            Some(&json!({
                "tool_slug": tool_name,
                "arguments": input,
                "account": account_id,
            })),
        )?;
        return Ok(res);
    }
    let endpoint = conn
        .pointer("/accessPaths/0/endpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| "Missing MCP endpoint.".to_string())?
        .to_string();
    let allow_local = conn
        .pointer("/accessPaths/0/local")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    check_public_https_url(&endpoint, false, allow_local)?;

    let (auth, token) = access_token_for_connection(store, &conn)?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(6))
        .timeout(Duration::from_secs(30))
        .build();
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "nod", "version": "0.1.0" }
        }
    });
    let (session_id, _) = mcp_post(&agent, &endpoint, &auth, &token, None, &init_req)
        .map_err(|e| redact_secret(&e, &token))?;
    let call_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": input
        }
    });
    let (_, resp) = mcp_post(&agent, &endpoint, &auth, &token, session_id.as_deref(), &call_req)
        .map_err(|e| redact_secret(&e, &token))?;
    let result = resp.get("result").cloned().unwrap_or(resp);
    let safe_str = redact_secret(&result.to_string(), &token);
    Ok(match serde_json::from_str::<Value>(&safe_str) {
        Ok(v) => v,
        Err(_) => Value::String(safe_str),
    })
}

pub fn search_connected_tools(store: &Store, query: &str, connection_filter: Option<&str>) -> Value {
    let q = query.trim().to_ascii_lowercase();
    let terms: Vec<&str> = q.split(|c: char| c.is_whitespace() || c == ',').filter(|s| !s.is_empty()).collect();
    let accounts: Vec<Value> = store
        .all("apps")
        .into_iter()
        .filter(|a| text(a, "status") == "connected")
        .filter(|a| connection_filter.map(|cid| text(a, "id") == cid).unwrap_or(true))
        .map(|a| {
            json!({
                "connectionId": text(&a, "id"),
                "service": text(&a, "slug"),
                "appName": text(&a, "name"),
                "accountName": text(&a, "accountName"),
            })
        })
        .collect();

    let all_tools = connected_mcp_tools(store);
    let mut matched = Vec::new();
    for spec in all_tools {
        if let Some(cid) = connection_filter
            && spec.connection_id != cid
        {
            continue;
        }
        let hay = format!(
            "{} {} {} {}",
            spec.service, spec.app_name, spec.tool_name, spec.description
        )
        .to_ascii_lowercase();
        let ok = terms.is_empty() || terms.iter().any(|t| hay.contains(t));
        if ok {
            matched.push(json!({
                "connectionId": spec.connection_id,
                "service": spec.service,
                "appName": spec.app_name,
                "tool": spec.tool_name,
                "description": spec.description,
                "inputSchema": spec.input_schema,
            }));
        }
        if matched.len() >= 25 {
            break;
        }
    }

    let mut search_error: Option<String> = None;
    let has_proxy_apps = store
        .all("apps")
        .iter()
        .any(|a| text(a, "status") == "connected" && text(a, "kind") == "composio_proxy");
    if matched.len() < 25 && !query.trim().is_empty() && has_proxy_apps {
        match ensure_composio_proxy_session(store, connection_filter) {
            Err(err) => search_error = Some(err),
            Ok((proxy_origin, sid)) => {
                let toolkits: Vec<String> = store
                    .all("apps")
                    .into_iter()
                    .filter(|a| text(a, "status") == "connected" && text(a, "kind") == "composio_proxy")
                    .filter(|a| connection_filter.map(|cid| text(a, "id") == cid).unwrap_or(true))
                    .map(|a| text(&a, "slug"))
                    .filter(|s| !s.is_empty())
                    .collect();
                if !toolkits.is_empty() {
                    let search_result = proxy_signed_request(
                        store,
                        &proxy_origin,
                        "POST",
                        &format!("/v2/api/v3.1/tool_router/session/{sid}/search"),
                        Some(&json!({
                            "query": query,
                            "toolkits": toolkits,
                        })),
                    );
                    match search_result {
                        Err(err) => search_error = Some(err),
                        Ok(search_res) => {
                            let schemas = search_res
                                .get("tool_schemas")
                                .or_else(|| search_res.get("toolSchemas"))
                                .and_then(Value::as_object);
                            if let Some(schemas) = schemas {
                                for (slug, tool) in schemas {
                                    let tk = tool
                                        .get("toolkit")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_ascii_lowercase();
                                    let cid = store
                                        .all("apps")
                                        .into_iter()
                                        .find(|a| {
                                            text(a, "kind") == "composio_proxy"
                                                && text(a, "slug") == tk
                                                && connection_filter
                                                    .map(|cid| text(a, "id") == cid)
                                                    .unwrap_or(true)
                                        })
                                        .map(|a| text(&a, "id"))
                                        .unwrap_or_default();
                                    if cid.is_empty() {
                                        continue;
                                    }
                                    matched.push(json!({
                                        "connectionId": cid,
                                        "service": tk,
                                        "appName": tk,
                                        "tool": slug,
                                        "description": tool.get("description").and_then(Value::as_str).unwrap_or_default(),
                                        "inputSchema": tool.get("input_schema").or_else(|| tool.get("inputSchema")).cloned().unwrap_or_else(|| json!({})),
                                    }));
                                    if matched.len() >= 25 {
                                        break;
                                    }
                                }
                            } else {
                                search_error = Some(
                                    "The app tool search returned an unexpected response.".to_string(),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    let mut result = json!({
        "accounts": accounts,
        "tools": matched,
    });
    if let Some(err) = search_error {
        result["error"] = json!(err);
    }
    result
}

fn strip_html_to_text(raw: &str) -> String {
    let mut cleaned = String::with_capacity(raw.len());
    let mut in_tag = false;
    let mut skip_block: Option<&'static str> = None;
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest_lower = raw[i..].to_ascii_lowercase();
        if let Some(end_tag) = skip_block {
            if let Some(pos) = rest_lower.find(end_tag) {
                i += pos + end_tag.len();
                skip_block = None;
                continue;
            }
            break;
        }
        if rest_lower.starts_with("<script") {
            skip_block = Some("</script>");
            i += 7;
            continue;
        }
        if rest_lower.starts_with("<style") {
            skip_block = Some("</style>");
            i += 6;
            continue;
        }
        let Some(ch) = raw[i..].chars().next() else {
            break;
        };
        let ch_len = ch.len_utf8();
        if ch == '<' {
            in_tag = true;
            if !cleaned.ends_with(' ') && !cleaned.ends_with('\n') {
                cleaned.push(' ');
            }
        } else if ch == '>' && in_tag {
            in_tag = false;
        } else if !in_tag {
            cleaned.push(ch);
        }
        i += ch_len;
    }
    let decoded = cleaned
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    let mut collapsed = String::new();
    let mut last_space = false;
    for ch in decoded.chars() {
        if ch == '\r' {
            continue;
        }
        if ch == ' ' || ch == '\t' {
            if !last_space {
                collapsed.push(' ');
                last_space = true;
            }
        } else {
            collapsed.push(ch);
            last_space = false;
        }
    }
    collapsed.trim().to_string()
}

pub fn fetch_public_url_text(raw_url: &str, max_chars: usize) -> Result<(String, String), String> {
    let mut current_url = check_public_https_url(raw_url, true, false)?.0;
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(8))
        .timeout(Duration::from_secs(15))
        .user_agent("nod")
        .build();

    for _hop in 0..=3 {
        let resp = agent
            .get(&current_url.normalized)
            .set("Accept", "text/html,application/json,text/plain,*/*;q=0.8")
            .call();
        match resp {
            Ok(r) => {
                if (300..400).contains(&r.status())
                    && let Some(loc) = r.header("Location")
                {
                    let candidate = if loc.starts_with("https://") || loc.starts_with("http://") {
                        loc.to_string()
                    } else if loc.starts_with('/') {
                        format!("{}{}", current_url.origin, loc)
                    } else {
                        return Err("Unsupported relative redirect from remote host.".to_string());
                    };
                    current_url = check_public_https_url(&candidate, true, false)?.0;
                    continue;
                }
                let ctype = r.content_type().to_ascii_lowercase();
                if !ctype.is_empty()
                    && !ctype.contains("text")
                    && !ctype.contains("json")
                    && !ctype.contains("xml")
                    && !ctype.contains("javascript")
                {
                    return Err(format!("Unsupported content type from remote host: {ctype}"));
                }
                let mut buf = Vec::new();
                r.into_reader()
                    .take(512 * 1024)
                    .read_to_end(&mut buf)
                    .map_err(|e| format!("Failed reading remote response: {e}"))?;
                let raw_text = String::from_utf8_lossy(&buf).to_string();
                let cleaned = if ctype.contains("html") {
                    strip_html_to_text(&raw_text)
                } else {
                    raw_text.trim().to_string()
                };
                let limit = max_chars.clamp(500, 32_000);
                let out: String = if cleaned.chars().count() > limit {
                    format!(
                        "{}\n\n[Truncated]",
                        cleaned.chars().take(limit).collect::<String>()
                    )
                } else {
                    cleaned
                };
                return Ok((current_url.normalized, out));
            }
            Err(ureq::Error::Status(code, r)) => {
                let snippet: String = r.into_string().unwrap_or_default().chars().take(240).collect();
                return Err(format!("Remote host returned HTTP {code}: {snippet}"));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(format!("Could not fetch {}: {t}", current_url.normalized));
            }
        }
    }
    Err("Too many redirects while fetching URL.".to_string())
}

pub fn connected_apps_summary(store: &Store) -> String {
    let apps = store.all("apps");
    if apps.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = apps
        .iter()
        .filter(|a| text(a, "status") == "connected")
        .map(|a| {
            let name = text(a, "name");
            let account = text(a, "accountName");
            let tools = a
                .get("tools")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            if tools.is_empty() {
                format!("- {name} ({account})")
            } else {
                format!("- {name} ({account}) [MCP tools: {tools}]")
            }
        })
        .collect();
    if lines.is_empty() {
        String::new()
    } else {
        format!("Connected external apps / MCP servers:\n{}", lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn proxy_origin_rejects_cleartext_private_and_credentialed_addresses() {
        assert!(validated_proxy_origin("http://169.254.169.254").is_err());
        assert!(validated_proxy_origin("http://10.0.0.4:8080").is_err());
        assert!(validated_proxy_origin("https://user:pass@composio.n8n.cloud").is_err());
        assert!(validated_proxy_origin("not a url").is_err());
        assert_eq!(
            validated_proxy_origin("http://127.0.0.1:8787").unwrap(),
            "http://127.0.0.1:8787"
        );
    }

    #[test]
    fn catalog_pagination_reports_remote_failures_instead_of_empty_results() {
        let missing_items = fetch_composio_pages(|_| Ok(json!({ "unexpected": true })));
        assert!(missing_items.is_err());
        assert!(missing_items.unwrap_err().contains("toolkit list"));

        let page = std::cell::Cell::new(0_u32);
        let never_ends = fetch_composio_pages(|_| {
            let next = page.get() + 1;
            page.set(next);
            Ok(json!({
                "items": [],
                "next_cursor": format!("cursor-{next}")
            }))
        });
        assert!(never_ends.is_err());
        assert!(never_ends.unwrap_err().contains("every page"));
    }

    use super::*;
    use crate::runtime::RuntimeManager;
    use crate::server::Live;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::thread;

    fn call(
        store: &Arc<Store>,
        live: &Arc<Live>,
        runtime: &Arc<RuntimeManager>,
        method: &str,
        path: &str,
        body: Value,
    ) -> Reply {
        let query = HashMap::new();
        let ctx = Ctx {
            method,
            path,
            query: &query,
            body,
            body_bytes: &[],
            content_type: "application/json",
            store,
            live,
            runtime,
            owner: "owner",
        };
        handle(&ctx).expect("handled")
    }

    static PROXY_TEST_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[test]
    fn apps_bundled_catalog_ssrf_and_live_mcp_verification_work() {
        let _env_guard = PROXY_TEST_ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        if let Ok(mut g) = composio_cache().lock() {
            *g = None;
        }
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let live = Arc::new(Live::default());
        let runtime = RuntimeManager::new(Arc::clone(&live));

        let cat = call(&store, &live, &runtime, "GET", "/api/apps/catalog", Value::Null);
        assert_eq!(cat.status, 200);
        let items = cat.body.as_array().unwrap();
        let slugs: Vec<&str> = items.iter().filter_map(|i| i["slug"].as_str()).collect();
        assert_eq!(slugs.len(), 3);
        assert!(slugs.contains(&"linear"));
        assert!(slugs.contains(&"n8n"));
        assert!(slugs.contains(&"notion"));

        let icon = call(&store, &live, &runtime, "GET", "/api/apps/catalog/linear/icon", Value::Null);
        assert_eq!(icon.status, 200);
        let (bytes, mime) = icon.raw.unwrap();
        assert_eq!(mime, "image/svg+xml");
        assert!(String::from_utf8_lossy(&bytes).contains("<svg"));

        let ssrf_bad = call(
            &store,
            &live,
            &runtime,
            "POST",
            "/api/apps/mcp",
            json!({ "name": "Bad", "endpoint": "https://169.254.169.254/mcp", "auth": "bearer" }),
        );
        assert_eq!(ssrf_bad.status, 400);

        let server = tiny_http::Server::http("127.0.0.1:0").expect("mock mcp");
        let port = server.server_addr().to_ip().unwrap().port();
        let srv_handle = thread::spawn(move || {
            for mut req in server.incoming_requests().take(6) {
                let mut buf = String::new();
                let _ = req.as_reader().read_to_string(&mut buf);
                let parsed: Value = serde_json::from_str(&buf).unwrap_or(Value::Null);
                let method = parsed["method"].as_str().unwrap_or("");
                let auth_hdr = req
                    .headers()
                    .iter()
                    .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("Authorization"))
                    .map(|h| h.value.as_str().to_string())
                    .unwrap_or_default();
                if auth_hdr != "Bearer valid-secret-123" {
                    let resp = tiny_http::Response::from_string("unauthorized").with_status_code(401);
                    let _ = req.respond(resp);
                    continue;
                }
                let reply_json = match method {
                    "initialize" => json!({ "jsonrpc": "2.0", "id": parsed["id"], "result": { "protocolVersion": "2024-11-05" } }),
                    "tools/list" => json!({
                        "jsonrpc": "2.0",
                        "id": parsed["id"],
                        "result": { "tools": [{ "name": "ping_tool", "description": "Ping", "inputSchema": { "type": "object" } }] }
                    }),
                    "tools/call" => json!({
                        "jsonrpc": "2.0",
                        "id": parsed["id"],
                        "result": { "content": [{ "type": "text", "text": "pong valid-secret-123" }] }
                    }),
                    _ => json!({ "jsonrpc": "2.0", "id": parsed["id"], "result": {} }),
                };
                let hdr = tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap();
                let resp = tiny_http::Response::from_string(reply_json.to_string()).with_header(hdr);
                let _ = req.respond(resp);
            }
        });

        let endpoint = format!("http://127.0.0.1:{port}/mcp");
        let mcp = call(
            &store,
            &live,
            &runtime,
            "POST",
            "/api/apps/mcp",
            json!({ "name": "Local Docs", "endpoint": endpoint, "auth": "bearer" }),
        );
        assert_eq!(mcp.status, 200);
        let disc_id = mcp.body["id"].as_str().unwrap();

        let bad_tok = call(
            &store,
            &live,
            &runtime,
            "POST",
            &format!("/api/apps/discovery/{disc_id}/complete"),
            json!({ "accountName": "Work", "token": "wrong-token" }),
        );
        assert_eq!(bad_tok.status, 409);

        let good = call(
            &store,
            &live,
            &runtime,
            "POST",
            &format!("/api/apps/discovery/{disc_id}/complete"),
            json!({ "accountName": "Work", "token": "valid-secret-123" }),
        );
        assert_eq!(good.status, 200);
        assert!(good.body.get("tokenEnc").is_none());
        let conn_id = good.body["id"].as_str().unwrap();

        let tools = connected_mcp_tools(store.as_ref());
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_name, "ping_tool");

        let out = call_connected_mcp_tool(store.as_ref(), conn_id, "ping_tool", json!({})).expect("mcp call");
        assert!(!out.to_string().contains("valid-secret-123"));
        assert!(out.to_string().contains("[redacted]"));

        drop(srv_handle);
    }

    #[test]
    fn composio_proxy_catalog_connect_and_tool_call_work() {
        let _env_guard = PROXY_TEST_ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        struct EnvReset(Option<String>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(v) => std::env::set_var("NOD_COMPOSIO_PROXY_URL", v),
                        None => std::env::remove_var("NOD_COMPOSIO_PROXY_URL"),
                    }
                }
            }
        }
        let _reset = EnvReset(std::env::var("NOD_COMPOSIO_PROXY_URL").ok());

        if let Ok(mut g) = composio_cache().lock() {
            *g = None;
        }
        if let Ok(mut g) = proxy_enrolled().lock() {
            g.clear();
        }
        if let Ok(mut g) = proxy_sessions().lock() {
            g.clear();
        }
        if let Ok(mut g) = proxy_attempts().lock() {
            g.clear();
        }

        let store = Arc::new(Store::open_in_memory().expect("store"));
        let live = Arc::new(Live::default());
        let runtime = RuntimeManager::new(Arc::clone(&live));
        let identity = load_or_create_proxy_identity(store.as_ref()).expect("identity");
        let expected_pk = identity.public_key;
        let expected_uid = identity.user_id;

        let server = tiny_http::Server::http("127.0.0.1:0").expect("mock proxy");
        let port = server.server_addr().to_ip().unwrap().port();
        let proxy_url = format!("http://127.0.0.1:{port}");
        unsafe {
            std::env::set_var("NOD_COMPOSIO_PROXY_URL", &proxy_url);
        }

        let proxy_url_srv = proxy_url.clone();
        let srv_handle = thread::spawn(move || {
            for mut req in server.incoming_requests().take(7) {
                let mut body_buf = String::new();
                let _ = req.as_reader().read_to_string(&mut body_buf);
                let parsed: Value = serde_json::from_str(&body_buf).unwrap_or(Value::Null);
                let pk_hdr = req
                    .headers()
                    .iter()
                    .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("x-nod-public-key"))
                    .map(|h| h.value.as_str().to_string())
                    .unwrap_or_default();
                let sig_hdr = req
                    .headers()
                    .iter()
                    .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("x-nod-signature"))
                    .map(|h| h.value.as_str().to_string())
                    .unwrap_or_default();
                assert_eq!(pk_hdr, expected_pk);
                assert!(!sig_hdr.is_empty());

                let path = req.url().to_string();
                let resp_val = if path == "/v2/access/enroll" {
                    json!({ "state": "enabled", "userId": expected_uid })
                } else if path.starts_with("/v2/api/v3.1/toolkits") {
                    json!({
                        "items": [{
                            "slug": "gmail",
                            "name": "Gmail",
                            "meta": {
                                "description": "Read and send email.",
                                "categories": [{ "name": "Communication" }],
                                "logo": "https://logos.composio.dev/api/gmail"
                            },
                            "composio_managed_auth_schemes": ["OAUTH2"]
                        }],
                        "next_cursor": null
                    })
                } else if path == "/v2/connect/start" {
                    assert_eq!(parsed["toolkit"].as_str(), Some("gmail"));
                    json!({
                        "transactionId": parsed["transactionId"],
                        "redirectUrl": format!("{proxy_url_srv}/v2/connect/pair?ticket=tkt-1")
                    })
                } else if path == "/v2/connect/complete" {
                    assert_eq!(parsed["completionToken"].as_str(), Some("tok-1"));
                    json!({
                        "toolkit": "gmail",
                        "status": "ACTIVE",
                        "connectedAccountId": "ca-gmail-1"
                    })
                } else if path == "/v2/api/v3.1/tool_router/session" {
                    json!({ "session_id": "sess-1" })
                } else if path == "/v2/api/v3.1/tool_router/session/sess-1/search" {
                    json!({
                        "tool_schemas": {
                            "GMAIL_FETCH_EMAILS": {
                                "toolkit": "gmail",
                                "description": "Fetch emails",
                                "input_schema": { "type": "object" }
                            }
                        }
                    })
                } else if path == "/v2/api/v3.1/tool_router/session/sess-1/execute" {
                    assert_eq!(parsed["tool_slug"].as_str(), Some("GMAIL_FETCH_EMAILS"));
                    assert_eq!(parsed["account"].as_str(), Some("ca-gmail-1"));
                    json!({ "data": { "messages": ["hello"] }, "successful": true })
                } else {
                    json!({})
                };
                let hdr = tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap();
                let resp = tiny_http::Response::from_string(resp_val.to_string()).with_header(hdr);
                let _ = req.respond(resp);
            }
        });

        let cat = call(&store, &live, &runtime, "GET", "/api/apps/catalog", Value::Null);
        assert_eq!(cat.status, 200);
        let items = cat.body.as_array().unwrap();
        assert!(items.iter().any(|i| i["slug"].as_str() == Some("gmail")));
        assert!(items.iter().any(|i| i["slug"].as_str() == Some("linear")));

        let started = call(
            &store,
            &live,
            &runtime,
            "POST",
            "/api/apps/gmail/connect",
            json!({ "returnTo": "apps" }),
        );
        assert_eq!(started.status, 200);
        let connect_url = started.body["url"].as_str().unwrap();
        let attempt = started.body["attempt"].as_str().unwrap().to_string();
        let cap = connect_url.rsplit('/').next().unwrap().to_string();
        let cap_path = format!("/api/apps/connect/{cap}");

        let bridge_html = call(&store, &live, &runtime, "GET", &cap_path, Value::Null);
        assert_eq!(bridge_html.status, 200);

        let bridge_key = proxy_attempts()
            .lock()
            .unwrap()
            .get(&cap)
            .unwrap()
            .bridge_key
            .clone();
        let mut start_query = HashMap::new();
        start_query.insert("__x_nod_bridge".to_string(), bridge_key);
        start_query.insert("__req_origin".to_string(), live.origin().to_string());
        let b64_43 = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let start_ctx = Ctx {
            method: "POST",
            path: &cap_path,
            query: &start_query,
            body: json!({
                "browserKey": { "kty": "EC", "crv": "P-256", "x": b64_43, "y": b64_43 }
            }),
            body_bytes: &[],
            content_type: "application/json",
            store: &store,
            live: &live,
            runtime: &runtime,
            owner: "owner",
        };
        let bridge_post = handle(&start_ctx).expect("bridge post");
        assert_eq!(bridge_post.status, 200);
        assert!(bridge_post.body["url"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{proxy_url}/v2/connect/pair?ticket=")));

        let mut cb_query = HashMap::new();
        cb_query.insert("state".to_string(), attempt);
        cb_query.insert("completion_token".to_string(), "tok-1".to_string());
        let cb_ctx = Ctx {
            method: "GET",
            path: "/api/apps/callback",
            query: &cb_query,
            body: Value::Null,
            body_bytes: &[],
            content_type: "application/json",
            store: &store,
            live: &live,
            runtime: &runtime,
            owner: "owner",
        };
        let cb_reply = handle(&cb_ctx).expect("callback");
        assert_eq!(cb_reply.status, 302);

        let searched = search_connected_tools(store.as_ref(), "emails", None);
        let tools = searched["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["tool"].as_str() == Some("GMAIL_FETCH_EMAILS")));
        let conn_id = tools
            .iter()
            .find(|t| t["tool"].as_str() == Some("GMAIL_FETCH_EMAILS"))
            .unwrap()["connectionId"]
            .as_str()
            .unwrap()
            .to_string();

        let executed = call_connected_mcp_tool(
            store.as_ref(),
            &conn_id,
            "GMAIL_FETCH_EMAILS",
            json!({ "limit": 5 }),
        )
        .expect("execute proxy tool");
        assert_eq!(executed["successful"], json!(true));

        let _ = srv_handle.join();
    }
}
