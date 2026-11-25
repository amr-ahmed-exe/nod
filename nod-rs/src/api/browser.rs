//! Private browser lifecycle endpoints and trust-gated action dispatch.

use super::computer_use::{TrustCheckInput, TrustDecision, TrustGuard};
use super::{Ctx, Reply};
use crate::runtime::errors::Redactor;
use crate::runtime::storage;
use serde_json::{json, Value};
use std::time::Duration;

const BRIDGE_SCRIPT: &str = "/app/bridge.mjs";

/// Handles `/api/browser` and `/api/browser-handoff` routes strictly against live runtime health.
pub fn handle(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    if path == "/api/browser" && method == "GET" {
        return Some(status(ctx));
    }
    if path == "/api/browser/screen" && method == "GET" {
        return Some(screenshot(ctx));
    }
    if path == "/api/browser/action" && method == "POST" {
        return Some(action(ctx));
    }
    if path == "/api/browser-handoff" && method == "POST" {
        return Some(handoff_request(ctx));
    }
    if path == "/api/browser-handoff/complete" && method == "POST" {
        return Some(handoff_complete(ctx));
    }
    if path.starts_with("/api/browser") || path.starts_with("/api/browser-handoff") {
        return Some(Reply::json(
            501,
            json!({
                "error": "This private browser operation is not available yet.",
                "status": "setup_required",
                "ready": false
            }),
        ));
    }
    None
}

fn unready_reply(ctx: &Ctx) -> Reply {
    let runtime_status = ctx.runtime.status_json();
    let msg = runtime_status
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| runtime_status.get("message").and_then(Value::as_str))
        .unwrap_or("Nod's private browser is not ready yet.");
    Reply::json(
        503,
        json!({
            "error": msg,
            "status": "setup_required",
            "ready": false,
            "connected": false,
            "running": false,
            "tabs": [],
            "activeTabId": Value::Null,
            "runtime": runtime_status
        }),
    )
}

fn status(ctx: &Ctx) -> Reply {
    if !ctx.runtime.browser_ready() {
        return unready_reply(ctx);
    }
    let out = match ctx
        .runtime
        .exec_browser_command(&["node", BRIDGE_SCRIPT, "status"], None, Duration::from_secs(10))
    {
        Ok(out) => out,
        Err(err) => {
            return Reply::json(
                503,
                json!({
                    "error": err.message,
                    "status": "setup_required",
                    "ready": false,
                    "connected": false,
                    "running": false
                }),
            );
        }
    };
    if !out.success() {
        let redactor = Redactor::default();
        return Reply::json(
            503,
            json!({
                "error": "Private browser bridge did not answer healthily.",
                "details": redactor.message(&out.combined()),
                "status": "setup_required",
                "ready": false,
                "connected": false,
                "running": false
            }),
        );
    }
    let parsed: Value = serde_json::from_str(&out.stdout).unwrap_or_else(|_| json!({}));
    let tabs = parsed.get("tabs").cloned().unwrap_or_else(|| json!([]));
    let active_tab_id = parsed.get("activeTabId").cloned().unwrap_or(Value::Null);
    Reply::ok(json!({
        "owner": ctx.owner,
        "ready": true,
        "connected": true,
        "running": true,
        "status": "ready",
        "tabs": tabs,
        "activeTabId": active_tab_id,
    }))
}

fn screenshot(ctx: &Ctx) -> Reply {
    if !ctx.runtime.browser_ready() {
        return unready_reply(ctx);
    }
    let out = match ctx.runtime.exec_browser_command(
        &["node", BRIDGE_SCRIPT, "screenshot"],
        None,
        Duration::from_secs(12),
    ) {
        Ok(out) => out,
        Err(err) => {
            return Reply::json(
                503,
                json!({ "error": err.message, "status": "setup_required", "ready": false }),
            );
        }
    };
    if !out.success() {
        return Reply::json(
            503,
            json!({ "error": "Could not capture private browser screen.", "status": "setup_required" }),
        );
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(&out.stdout)
        && let Some(b64) = parsed.get("pngBase64").and_then(Value::as_str)
            && let Some(bytes) = decode_base64(b64) {
                return Reply::bytes(200, bytes, "image/png".to_string());
            }
    Reply::json(
        503,
        json!({ "error": "Private browser screenshot returned empty image.", "status": "setup_required" }),
    )
}

fn action(ctx: &Ctx) -> Reply {
    if !ctx.runtime.browser_ready() {
        return unready_reply(ctx);
    }
    let kind = ctx
        .body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if kind.is_empty() {
        return Reply::error(400, "Missing browser action.");
    }
    let approval_token = ctx
        .body
        .get("approvalToken")
        .and_then(Value::as_str)
        .map(str::to_string);
    let target = ctx
        .body
        .get("url")
        .or_else(|| ctx.body.get("selector"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let input = TrustCheckInput {
        owner: ctx.owner.to_string(),
        surface: "browser".to_string(),
        action: kind.to_string(),
        target,
        args: ctx.body.clone(),
        approval_token,
        is_password_field: ctx
            .body
            .get("isPasswordField")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    match TrustGuard::verify(ctx.store, &input) {
        TrustDecision::Allowed => {}
        TrustDecision::ApprovalRequired { request_id, reason } => {
            return Reply::json(
                403,
                json!({
                    "error": reason,
                    "status": "approval_required",
                    "requestId": request_id,
                    "retryable": false
                }),
            );
        }
        TrustDecision::Denied { reason } => {
            return Reply::json(
                403,
                json!({
                    "error": reason,
                    "status": "blocked",
                    "retryable": false
                }),
            );
        }
    }

    let payload = json!({
        "owner": ctx.owner,
        "action": kind,
        "args": redact_sensitive_args(&ctx.body),
    });
    let serialized = payload.to_string();
    let out = match ctx.runtime.exec_browser_command(
        &["node", BRIDGE_SCRIPT, "action"],
        Some(&serialized),
        Duration::from_secs(20),
    ) {
        Ok(out) => out,
        Err(err) => {
            return Reply::json(
                503,
                json!({
                    "error": err.message,
                    "status": "setup_required",
                    "retryable": false
                }),
            );
        }
    };
    if !out.success() {
        let redactor = Redactor::default();
        return Reply::json(
            502,
            json!({
                "error": "Private browser action did not confirm completion and was not automatically retried.",
                "details": redactor.message(&out.combined()),
                "retryable": false
            }),
        );
    }
    let result: Value = serde_json::from_str(&out.stdout).unwrap_or_else(|_| json!({ "ok": true }));
    Reply::ok(sanitize_browser_result(result))
}

fn handoff_request(ctx: &Ctx) -> Reply {
    if !ctx.runtime.browser_ready() {
        return unready_reply(ctx);
    }
    let reason = ctx
        .body
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("Manual confirmation required");
    let id = storage::random_hex(12);
    let record = json!({
        "id": id,
        "owner": ctx.owner,
        "reason": reason,
        "status": "pending",
        "createdAt": storage::now_iso()
    });
    ctx.store.put("browser_handoffs", &id, &record).ok();
    Reply::ok(record)
}

fn handoff_complete(ctx: &Ctx) -> Reply {
    let Some(id) = ctx.body.get("id").and_then(Value::as_str) else {
        return Reply::error(400, "Missing browser handoff id.");
    };
    let Some(mut existing) = ctx.store.get("browser_handoffs", id) else {
        return Reply::error(404, "Browser handoff request not found.");
    };
    if existing.get("owner").and_then(Value::as_str) != Some(ctx.owner) {
        return Reply::error(403, "Handoff request belongs to a different owner.");
    }
    if let Some(obj) = existing.as_object_mut() {
        obj.insert("status".to_string(), json!("completed"));
        obj.insert("completedAt".to_string(), json!(storage::now_iso()));
    }
    if let Err(err) = ctx.store.put("browser_handoffs", id, &existing) {
        return Reply::error(500, format!("Could not update handoff: {err}"));
    }
    Reply::ok(existing)
}

fn redact_sensitive_args(value: &Value) -> Value {
    let mut cloned = value.clone();
    if let Some(obj) = cloned.as_object_mut() {
        obj.remove("approvalToken");
        obj.remove("password");
        obj.remove("apiKey");
        obj.remove("sessionToken");
        obj.remove("cookie");
    }
    cloned
}

fn sanitize_browser_result(mut value: Value) -> Value {
    if let Some(obj) = value.as_object_mut() {
        for secret_key in ["password", "cookies", "cookie", "sessionToken", "apiKey", "token"] {
            obj.remove(secret_key);
        }
    }
    value
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    let mut i = 0;
    while i + 1 < clean.len() {
        let a = val(clean[i])?;
        let b = val(clean[i + 1])?;
        out.push((a << 2) | (b >> 4));
        if i + 2 < clean.len() {
            let c = val(clean[i + 2])?;
            out.push((b << 4) | (c >> 2));
            if i + 3 < clean.len() {
                let d = val(clean[i + 3])?;
                out.push((c << 6) | d);
            }
        }
        i += 4;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_secrets_from_browser_output() {
        let cleaned = sanitize_browser_result(json!({
            "ok": true,
            "title": "Example",
            "password": "p@ss",
            "sessionToken": "tok",
            "cookies": ["a=b"]
        }));
        assert_eq!(cleaned["ok"], json!(true));
        assert_eq!(cleaned["title"], json!("Example"));
        assert!(cleaned.get("password").is_none());
        assert!(cleaned.get("sessionToken").is_none());
        assert!(cleaned.get("cookies").is_none());
    }
}
