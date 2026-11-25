//! Computer-use lifecycle, owner-scoped settings, and unified TrustGuard safety enforcement.

use super::{Ctx, Reply};
use crate::db::Store;
use crate::runtime::storage;
use serde_json::{json, Value};

const BLOCKED_APPS: &[&str] = &[
    "1password",
    "bitwarden",
    "keepass",
    "lastpass",
    "dashlane",
    "keychain",
    "credential manager",
    "powershell",
    "cmd.exe",
    "command prompt",
    "terminal",
    "windowsterminal",
    "regedit",
    "taskmgr",
];

const APPROVAL_REQUIRED_ACTIONS: &[&str] = &[
    "commit",
    "sign_in",
    "signin",
    "login",
    "saved_session",
    "use_saved_session",
    "submit",
    "payment",
    "purchase",
    "checkout",
    "delete",
    "transfer",
    "send",
];

/// Input inspected by the unified trust check before allowing any computer-use or browser action.
#[derive(Debug, Clone)]
pub struct TrustCheckInput {
    pub owner: String,
    pub surface: String,
    pub action: String,
    pub target: Option<String>,
    pub args: Value,
    pub approval_token: Option<String>,
    pub is_password_field: bool,
}

/// Outcome returned by `TrustGuard::verify`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustDecision {
    Allowed,
    ApprovalRequired { request_id: String, reason: String },
    Denied { reason: String },
}

/// Shared security gate enforcing owner scoping, exact argument matching, and sensitive-target blocks.
pub struct TrustGuard;

impl TrustGuard {
    /// Computes canonical SHA-256 digest of `(owner, surface, action, target, args)`.
    pub fn args_digest(input: &TrustCheckInput) -> String {
        let mut sanitized = input.args.clone();
        if let Some(obj) = sanitized.as_object_mut() {
            obj.remove("approvalToken");
        }
        let canonical = json!({
            "owner": input.owner,
            "surface": input.surface,
            "action": input.action.to_ascii_lowercase(),
            "target": input.target,
            "args": sanitized,
        })
        .to_string();
        storage::sha256_bytes(canonical.as_bytes())
    }

    /// Verifies trust policy for a browser or computer-use operation.
    pub fn verify(store: &Store, input: &TrustCheckInput) -> TrustDecision {
        if input.is_password_field {
            return TrustDecision::Denied {
                reason: "Password and secret input fields are blocked from automation.".into(),
            };
        }
        if let Some(target) = &input.target {
            let lower = target.to_ascii_lowercase();
            if BLOCKED_APPS.iter().any(|app| lower.contains(app)) {
                return TrustDecision::Denied {
                    reason: "This application is restricted by Nod safety policy.".into(),
                };
            }
            let keep_out = keep_out_list(store, &input.owner);
            if keep_out
                .iter()
                .any(|rule| !rule.is_empty() && lower.contains(&rule.to_ascii_lowercase()))
            {
                return TrustDecision::Denied {
                    reason: "This app or site is in your Keep Out list.".into(),
                };
            }
        }

        let normalized_action = input.action.to_ascii_lowercase();
        let needs_approval = APPROVAL_REQUIRED_ACTIONS
            .iter()
            .any(|kind| normalized_action == *kind || normalized_action.contains(kind));
        if !needs_approval {
            return TrustDecision::Allowed;
        }

        let digest = Self::args_digest(input);
        if let Some(token) = &input.approval_token {
            if consume_matching_approval(store, &input.owner, token, &digest) {
                return TrustDecision::Allowed;
            }
            return TrustDecision::Denied {
                reason: "Approval token did not match the owner or exact action arguments.".into(),
            };
        }

        let request_id = storage::random_hex(12);
        let req = json!({
            "id": request_id,
            "owner": input.owner,
            "surface": input.surface,
            "action": input.action,
            "target": input.target,
            "digest": digest,
            "status": "pending",
            "approved": false,
            "createdAt": storage::now_iso()
        });
        if let Err(err) = store.put("trust_approvals", &request_id, &req) {
            return TrustDecision::Denied {
                reason: format!("Could not record approval request: {err}"),
            };
        }
        TrustDecision::ApprovalRequired {
            request_id,
            reason: format!(
                "Action '{}' requires explicit owner approval before proceeding.",
                input.action
            ),
        }
    }

    /// Marks a pending approval request approved by the same owner, producing a single-use token.
    pub fn approve(store: &Store, owner: &str, request_id: &str) -> Result<String, String> {
        let Some(mut req) = store.get("trust_approvals", request_id) else {
            return Err("Approval request not found.".into());
        };
        if req.get("owner").and_then(Value::as_str) != Some(owner) {
            return Err("Approval request belongs to a different owner.".into());
        }
        if req.get("status").and_then(Value::as_str) != Some("pending") {
            return Err("Approval request is no longer pending.".into());
        }
        let token = storage::random_hex(24);
        if let Some(obj) = req.as_object_mut() {
            obj.insert("status".to_string(), json!("approved"));
            obj.insert("approved".to_string(), json!(true));
            obj.insert("token".to_string(), json!(token));
            obj.insert("approvedAt".to_string(), json!(storage::now_iso()));
        }
        store.put("trust_approvals", request_id, &req)?;
        Ok(token)
    }
}

/// Handles `/api/computer-use*` endpoints using live runtime status and TrustGuard policy.
pub fn handle(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    match (method, path) {
        ("GET", "/api/computer-use") => Some(get_state(ctx)),
        ("PATCH", "/api/computer-use") => Some(patch_state(ctx)),
        ("POST", "/api/computer-use/permission") => Some(handle_permission(ctx)),
        ("POST", "/api/computer-use/keep-out") => Some(add_keep_out(ctx)),
        ("DELETE", "/api/computer-use/keep-out") => Some(remove_keep_out(ctx)),
        ("POST", "/api/computer-use/action") => Some(execute_action(ctx)),
        _ => None,
    }
}

fn settings_key(owner: &str) -> String {
    format!("computer_use:{owner}")
}

fn keep_out_key(owner: &str) -> String {
    format!("computer_use_keep_out:{owner}")
}

fn keep_out_list(store: &Store, owner: &str) -> Vec<String> {
    store
        .setting(&keep_out_key(owner))
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn computer_use_state(ctx: &Ctx) -> Value {
    let key = settings_key(ctx.owner);
    let saved = ctx.store.setting(&key).unwrap_or_else(|| json!({}));
    let enabled = saved
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let runtime_ready = ctx.runtime.is_ready();
    let keep_out = keep_out_list(ctx.store, ctx.owner);
    let pending: Vec<Value> = ctx
        .store
        .all("trust_approvals")
        .into_iter()
        .filter(|v| {
            v.get("owner").and_then(Value::as_str) == Some(ctx.owner)
                && v.get("status").and_then(Value::as_str) == Some("pending")
        })
        .map(|mut v| {
            if let Some(obj) = v.as_object_mut() {
                obj.remove("token");
            }
            v
        })
        .collect();

    let status = if !runtime_ready {
        "setup_required"
    } else if enabled {
        "ready"
    } else {
        "disabled"
    };

    let platform = if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };

    json!({
        "owner": ctx.owner,
        "platform": platform,
        "enabled": enabled,
        "available": runtime_ready,
        "ready": runtime_ready,
        "status": status,
        "allowAutomaticRetry": false,
        "trustedActionsOnly": true,
        "blockSensitiveActionsWithoutApproval": true,
        "requiresApprovalFor": APPROVAL_REQUIRED_ACTIONS,
        "keptOut": keep_out,
        "keepOut": keep_out,
        "permissions": {
            "accessibility": if runtime_ready { "not_required" } else { "unknown" },
            "screen": if runtime_ready { "not_required" } else { "unknown" },
            "ready": runtime_ready,
            "sandboxed": false
        },
        "pendingApprovals": pending,
        "runtime": ctx.runtime.status_json(),
    })
}

fn get_state(ctx: &Ctx) -> Reply {
    Reply::ok(computer_use_state(ctx))
}

fn patch_state(ctx: &Ctx) -> Reply {
    let key = settings_key(ctx.owner);
    let mut current = ctx
        .store
        .setting(&key)
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    if let Some(enabled) = ctx.body.get("enabled").and_then(Value::as_bool) {
        if enabled && !ctx.runtime.is_ready() {
            return Reply::json(
                409,
                json!({
                    "error": "Private computer runtime must be ready before enabling computer use.",
                    "status": "setup_required",
                    "ready": false,
                    "state": computer_use_state(ctx)
                }),
            );
        }
        current.insert("enabled".to_string(), json!(enabled));
    }
    ctx.store.set_setting(&key, &Value::Object(current));
    ctx.refresh();
    Reply::ok(computer_use_state(ctx))
}

fn handle_permission(ctx: &Ctx) -> Reply {
    let request_id = ctx
        .body
        .get("requestId")
        .or_else(|| ctx.body.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if request_id.is_empty() {
        return Reply::error(400, "Missing approval requestId.");
    }
    let approve = ctx
        .body
        .get("approved")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !approve {
        if let Some(mut req) = ctx.store.get("trust_approvals", request_id) {
            if req.get("owner").and_then(Value::as_str) != Some(ctx.owner) {
                return Reply::error(403, "Approval request belongs to a different owner.");
            }
            if let Some(obj) = req.as_object_mut() {
                obj.insert("status".to_string(), json!("denied"));
                obj.insert("approved".to_string(), json!(false));
            }
            if let Err(err) = ctx.store.put("trust_approvals", request_id, &req) {
                return Reply::error(500, err);
            }
        }
        return Reply::ok(json!({ "approved": false, "requestId": request_id }));
    }
    match TrustGuard::approve(ctx.store, ctx.owner, request_id) {
        Ok(token) => Reply::ok(json!({
            "approved": true,
            "requestId": request_id,
            "approvalToken": token
        })),
        Err(msg) => Reply::error(403, msg),
    }
}

fn add_keep_out(ctx: &Ctx) -> Reply {
    let target = ctx
        .body
        .get("target")
        .or_else(|| ctx.body.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if target.is_empty() {
        return Reply::error(400, "Missing keep-out target.");
    }
    let key = keep_out_key(ctx.owner);
    let mut items = keep_out_list(ctx.store, ctx.owner);
    if !items.iter().any(|existing| existing.eq_ignore_ascii_case(target)) {
        items.push(target.to_string());
    }
    ctx.store.set_setting(&key, &json!(items));
    ctx.refresh();
    Reply::ok(computer_use_state(ctx))
}

fn remove_keep_out(ctx: &Ctx) -> Reply {
    let target = ctx
        .body
        .get("target")
        .or_else(|| ctx.body.get("name"))
        .or_else(|| ctx.body.get("bundle"))
        .and_then(Value::as_str)
        .or_else(|| ctx.query.get("target").map(String::as_str))
        .map(str::trim)
        .unwrap_or("");
    let key = keep_out_key(ctx.owner);
    let mut items = keep_out_list(ctx.store, ctx.owner);
    items.retain(|existing| !existing.eq_ignore_ascii_case(target));
    ctx.store.set_setting(&key, &json!(items));
    ctx.refresh();
    Reply::ok(computer_use_state(ctx))
}

fn execute_action(ctx: &Ctx) -> Reply {
    if !ctx.runtime.is_ready() {
        return Reply::json(
            503,
            json!({
                "error": "Private computer runtime is not ready.",
                "status": "setup_required",
                "retryable": false
            }),
        );
    }
    let key = settings_key(ctx.owner);
    let enabled = ctx
        .store
        .setting(&key)
        .and_then(|v| v.get("enabled").and_then(Value::as_bool))
        .unwrap_or(false);
    if !enabled {
        return Reply::json(
            403,
            json!({
                "error": "Computer use is currently disabled.",
                "status": "disabled",
                "retryable": false
            }),
        );
    }

    let action = ctx
        .body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if action.is_empty() {
        return Reply::error(400, "Missing computer-use action.");
    }
    let input = TrustCheckInput {
        owner: ctx.owner.to_string(),
        surface: "computer-use".to_string(),
        action: action.to_string(),
        target: ctx.body.get("target").and_then(Value::as_str).map(str::to_string),
        args: ctx.body.clone(),
        approval_token: ctx
            .body
            .get("approvalToken")
            .and_then(Value::as_str)
            .map(str::to_string),
        is_password_field: ctx
            .body
            .get("isPasswordField")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    match TrustGuard::verify(ctx.store, &input) {
        TrustDecision::Allowed => Reply::json(
            501,
            json!({
                "error": "Desktop host pointer/keyboard driver is not connected in native preview.",
                "status": "setup_required",
                "retryable": false
            }),
        ),
        TrustDecision::ApprovalRequired { request_id, reason } => Reply::json(
            403,
            json!({
                "error": reason,
                "status": "approval_required",
                "requestId": request_id,
                "retryable": false
            }),
        ),
        TrustDecision::Denied { reason } => Reply::json(
            403,
            json!({
                "error": reason,
                "status": "blocked",
                "retryable": false
            }),
        ),
    }
}

fn consume_matching_approval(store: &Store, owner: &str, token: &str, digest: &str) -> bool {
    for mut item in store.all("trust_approvals") {
        let same_owner = item.get("owner").and_then(Value::as_str) == Some(owner);
        let is_approved = item.get("status").and_then(Value::as_str) == Some("approved");
        let same_token = item.get("token").and_then(Value::as_str) == Some(token);
        let same_digest = item.get("digest").and_then(Value::as_str) == Some(digest);
        if same_owner && is_approved && same_token && same_digest
            && let Some(id) = item.get("id").and_then(Value::as_str).map(str::to_string) {
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("status".to_string(), json!("consumed"));
                    obj.remove("token");
                    obj.insert("consumedAt".to_string(), json!(storage::now_iso()));
                }
                if store.put("trust_approvals", &id, &item).is_ok() {
                    return true;
                }
                return false;
            }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_guard_blocks_password_fields_and_sensitive_apps() {
        let store = Store::open_in_memory().unwrap();
        let pw_input = TrustCheckInput {
            owner: "owner-1".into(),
            surface: "browser".into(),
            action: "type".into(),
            target: Some("chrome".into()),
            args: json!({ "text": "hunter2" }),
            approval_token: None,
            is_password_field: true,
        };
        assert!(matches!(
            TrustGuard::verify(&store, &pw_input),
            TrustDecision::Denied { .. }
        ));

        let app_input = TrustCheckInput {
            owner: "owner-1".into(),
            surface: "computer-use".into(),
            action: "click".into(),
            target: Some("1Password".into()),
            args: json!({ "x": 10, "y": 10 }),
            approval_token: None,
            is_password_field: false,
        };
        assert!(matches!(
            TrustGuard::verify(&store, &app_input),
            TrustDecision::Denied { .. }
        ));
    }

    #[test]
    fn trust_guard_binds_approval_to_owner_and_exact_args() {
        let store = Store::open_in_memory().unwrap();
        let commit_input = TrustCheckInput {
            owner: "owner-a".into(),
            surface: "browser".into(),
            action: "commit".into(),
            target: Some("https://example.com/checkout".into()),
            args: json!({ "action": "commit", "amount": 10 }),
            approval_token: None,
            is_password_field: false,
        };
        let req_id = match TrustGuard::verify(&store, &commit_input) {
            TrustDecision::ApprovalRequired { request_id, .. } => request_id,
            other => panic!("Expected approval required, got {other:?}"),
        };

        assert!(TrustGuard::approve(&store, "owner-b", &req_id).is_err());
        let token = TrustGuard::approve(&store, "owner-a", &req_id).unwrap();

        let tampered = TrustCheckInput {
            args: json!({ "action": "commit", "amount": 9999 }),
            approval_token: Some(token.clone()),
            ..commit_input.clone()
        };
        assert!(matches!(
            TrustGuard::verify(&store, &tampered),
            TrustDecision::Denied { .. }
        ));

        let exact = TrustCheckInput {
            approval_token: Some(token.clone()),
            ..commit_input.clone()
        };
        assert_eq!(TrustGuard::verify(&store, &exact), TrustDecision::Allowed);
        // Single-use token must not replay!
        assert!(matches!(
            TrustGuard::verify(&store, &exact),
            TrustDecision::Denied { .. }
        ));
    }

    #[test]
    fn trust_guard_enforces_owner_keep_out_list() {
        let store = Store::open_in_memory().unwrap();
        store.set_setting(&keep_out_key("owner-x"), &json!(["my-bank.example"]));
        let input = TrustCheckInput {
            owner: "owner-x".into(),
            surface: "browser".into(),
            action: "navigate".into(),
            target: Some("https://my-bank.example/login".into()),
            args: json!({ "url": "https://my-bank.example/login" }),
            approval_token: None,
            is_password_field: false,
        };
        assert!(matches!(
            TrustGuard::verify(&store, &input),
            TrustDecision::Denied { .. }
        ));
    }
}
