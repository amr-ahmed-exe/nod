//! Encrypted local Vault storage backed by Windows DPAPI (`CryptProtectData`) and SQLite.

use super::{text, Ctx, Reply};
use crate::db::Store;
use crate::util::{iso_now, now, uuid};
use chrono::Duration;
use serde_json::{json, Value};
use std::collections::HashSet;

#[cfg(windows)]
mod dpapi {
    use core::ffi::c_void;

    #[repr(C)]
    struct CryptIntegerBlob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    unsafe extern "system" {
        fn CryptProtectData(
            p_data_in: *const CryptIntegerBlob,
            sz_data_descr: *const u16,
            p_optional_entropy: *const CryptIntegerBlob,
            pv_reserved: *const c_void,
            p_prompt_struct: *const c_void,
            dw_flags: u32,
            p_data_out: *mut CryptIntegerBlob,
        ) -> i32;

        fn CryptUnprotectData(
            p_data_in: *const CryptIntegerBlob,
            ppsz_data_descr: *mut *mut u16,
            p_optional_entropy: *const CryptIntegerBlob,
            pv_reserved: *const c_void,
            p_prompt_struct: *const c_void,
            dw_flags: u32,
            p_data_out: *mut CryptIntegerBlob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(hmem: *mut c_void) -> *mut c_void;
    }

    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x01;

    pub fn protect(plain: &str) -> Result<String, String> {
        if plain.is_empty() {
            return Ok(String::new());
        }
        let mut bytes = plain.as_bytes().to_vec();
        let input = CryptIntegerBlob {
            cb_data: u32::try_from(bytes.len()).map_err(|_| "Secret is too large.".to_string())?,
            pb_data: bytes.as_mut_ptr(),
        };
        let mut out = CryptIntegerBlob {
            cb_data: 0,
            pb_data: core::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptProtectData(
                &input,
                core::ptr::null(),
                core::ptr::null(),
                core::ptr::null(),
                core::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 || out.pb_data.is_null() {
            return Err("Windows credential encryption failed.".to_string());
        }
        let cipher = unsafe { core::slice::from_raw_parts(out.pb_data, out.cb_data as usize) }.to_vec();
        unsafe { LocalFree(out.pb_data.cast()) };
        Ok(format!("dpapi:{}", hex_encode(&cipher)))
    }

    pub fn unprotect(encoded: &str) -> Result<String, String> {
        if encoded.is_empty() {
            return Ok(String::new());
        }
        let Some(hex) = encoded.strip_prefix("dpapi:") else {
            return Ok(encoded.to_string());
        };
        let mut cipher = hex_decode(hex)?;
        let input = CryptIntegerBlob {
            cb_data: u32::try_from(cipher.len()).map_err(|_| "Ciphertext is too large.".to_string())?,
            pb_data: cipher.as_mut_ptr(),
        };
        let mut out = CryptIntegerBlob {
            cb_data: 0,
            pb_data: core::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                core::ptr::null_mut(),
                core::ptr::null(),
                core::ptr::null(),
                core::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 || out.pb_data.is_null() {
            return Err("Windows credential decryption failed.".to_string());
        }
        let plain = unsafe { core::slice::from_raw_parts(out.pb_data, out.cb_data as usize) }.to_vec();
        unsafe { LocalFree(out.pb_data.cast()) };
        String::from_utf8(plain).map_err(|_| "Stored secret is not valid UTF-8.".to_string())
    }

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn hex_decode(hex: &str) -> Result<Vec<u8>, String> {
        if !hex.len().is_multiple_of(2) {
            return Err("Invalid encrypted secret encoding.".to_string());
        }
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| "Invalid hex.".to_string()))
            .collect()
    }
}

#[cfg(not(windows))]
mod dpapi {
    pub fn protect(plain: &str) -> Result<String, String> {
        Ok(plain.to_string())
    }

    pub fn unprotect(encoded: &str) -> Result<String, String> {
        Ok(encoded.to_string())
    }
}

/// Encrypts a secret string using the current user's Windows DPAPI context.
pub fn encrypt_secret(plain: &str) -> Result<String, String> {
    dpapi::protect(plain)
}

/// Decrypts a DPAPI-protected secret string for the current Windows user.
pub fn decrypt_secret(encoded: &str) -> Result<String, String> {
    dpapi::unprotect(encoded)
}

/// Handles `/api/vault*`, `/api/credential-requests*`, and `/api/payment-requests*`.
pub fn handle(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    if path == "/api/credential-requests" && method == "GET" {
        return Some(Reply::ok(json!([])));
    }
    if path == "/api/payment-requests" && method == "GET" {
        return Some(Reply::ok(json!([])));
    }

    if path == "/api/vault" {
        return match method {
            "GET" => Some(list_entries(ctx)),
            "POST" => Some(create_entry(ctx)),
            _ => None,
        };
    }

    if path == "/api/vault/import" && method == "POST" {
        return Some(import_logins(ctx));
    }
    if path == "/api/vault/sessions" && method == "GET" {
        return Some(list_sessions(ctx));
    }
    if path == "/api/vault/activity" && method == "GET" {
        return Some(list_activity(ctx));
    }

    let rest = path.strip_prefix("/api/vault/")?;
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match (method, parts.as_slice()) {
        ("DELETE", ["sessions", id]) => Some(delete_session(ctx, id)),
        ("POST", [id, "details"]) => Some(entry_details(ctx, id)),
        ("POST", [id, "password"]) => Some(reveal_secret(ctx, id, false)),
        ("POST", [id, "card-number"]) => Some(reveal_secret(ctx, id, true)),
        ("GET", [id, "icon"]) => Some(entry_icon(ctx, id)),
        ("PATCH", [id]) => Some(update_entry(ctx, id)),
        ("DELETE", [id]) => Some(delete_entry(ctx, id)),
        _ => None,
    }
}

fn normalize_origin(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

fn host_label(origin: &str) -> String {
    let clean = origin
        .split("://")
        .nth(1)
        .unwrap_or(origin)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(origin);
    clean.strip_prefix("www.").unwrap_or(clean).to_string()
}

fn username_hint(username: &str) -> String {
    let u = username.trim();
    if u.is_empty() {
        return String::new();
    }
    if let Some((local, domain)) = u.split_once('@') {
        let prefix: String = local.chars().take(2).collect();
        return format!("{prefix}...@{domain}");
    }
    let chars: Vec<char> = u.chars().collect();
    if chars.len() <= 3 {
        u.to_string()
    } else {
        format!("{}...", chars.into_iter().take(3).collect::<String>())
    }
}

fn detect_card_brand(digits: &str) -> &'static str {
    if digits.starts_with('4') {
        "Visa"
    } else if digits.starts_with("34") || digits.starts_with("37") {
        "American Express"
    } else if digits.starts_with("51")
        || digits.starts_with("52")
        || digits.starts_with("53")
        || digits.starts_with("54")
        || digits.starts_with("55")
        || digits.starts_with("22")
    {
        "Mastercard"
    } else if digits.starts_with("6011") || digits.starts_with("65") {
        "Discover"
    } else {
        "Card"
    }
}

fn entry_public_view(entry: &Value) -> Value {
    let kind = text(entry, "kind");
    if kind == "card" {
        json!({
            "kind": "card",
            "id": text(entry, "id"),
            "label": text(entry, "label"),
            "brand": text(entry, "brand"),
            "last4": text(entry, "last4"),
            "expiryMonth": entry.get("expiryMonth").and_then(Value::as_u64).unwrap_or(1),
            "expiryYear": entry.get("expiryYear").and_then(Value::as_u64).unwrap_or(2030),
            "updatedAt": text(entry, "updatedAt"),
        })
    } else {
        let username = text(entry, "username");
        json!({
            "kind": "login",
            "id": text(entry, "id"),
            "label": text(entry, "label"),
            "origin": text(entry, "origin"),
            "usernameHint": username_hint(&username),
            "username": username,
            "updatedAt": text(entry, "updatedAt"),
        })
    }
}

fn entry_details_view(entry: &Value) -> Value {
    let kind = text(entry, "kind");
    if kind == "card" {
        json!({
            "kind": "card",
            "id": text(entry, "id"),
            "label": text(entry, "label"),
            "brand": text(entry, "brand"),
            "last4": text(entry, "last4"),
            "expiryMonth": entry.get("expiryMonth").and_then(Value::as_u64).unwrap_or(1),
            "expiryYear": entry.get("expiryYear").and_then(Value::as_u64).unwrap_or(2030),
            "cardholder": text(entry, "cardholder"),
            "updatedAt": text(entry, "updatedAt"),
        })
    } else {
        let username = text(entry, "username");
        json!({
            "kind": "login",
            "id": text(entry, "id"),
            "label": text(entry, "label"),
            "origin": text(entry, "origin"),
            "usernameHint": username_hint(&username),
            "username": username,
            "hasPassword": !text(entry, "passwordEnc").is_empty(),
            "hasAuthenticator": !text(entry, "authenticatorEnc").is_empty(),
            "updatedAt": text(entry, "updatedAt"),
        })
    }
}

fn record_activity(store: &Store, action: &str, origin: &str, item_id: &str, item_kind: &str, task: &str) {
    let id = uuid();
    let doc = json!({
        "id": id,
        "at": iso_now(),
        "action": action,
        "origin": origin,
        "itemId": item_id,
        "itemKind": item_kind,
        "task": task,
        "reason": "owner",
    });
    let _ = store.put("vault_activity", &id, &doc);
}

fn notify_vault(ctx: &Ctx) {
    ctx.live.notify("vault");
    ctx.refresh();
}

fn list_entries(ctx: &Ctx) -> Reply {
    let views: Vec<Value> = ctx
        .store
        .all("vault_entries")
        .iter()
        .map(entry_public_view)
        .collect();
    Reply::ok(Value::Array(views))
}

fn create_entry(ctx: &Ctx) -> Reply {
    let kind = text(&ctx.body, "kind");
    let id = uuid();
    let now_iso = iso_now();

    if kind == "card" {
        let raw_num = text(&ctx.body, "cardNumber");
        let digits: String = raw_num.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() < 12 || digits.len() > 19 {
            return Reply::error(400, "Enter a valid card number.");
        }
        let month = ctx
            .body
            .get("expiryMonth")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let year = ctx
            .body
            .get("expiryYear")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if !(1..=12).contains(&month) || !(2000..=2199).contains(&year) {
            return Reply::error(400, "Enter a valid expiry month and year.");
        }
        let cardholder = text(&ctx.body, "cardholder").trim().to_string();
        let last4 = digits[digits.len().saturating_sub(4)..].to_string();
        let brand = detect_card_brand(&digits);
        let label = {
            let l = text(&ctx.body, "label").trim().to_string();
            if l.is_empty() {
                format!("{brand} ending in {last4}")
            } else {
                l
            }
        };
        let enc_num = match encrypt_secret(&digits) {
            Ok(v) => v,
            Err(e) => return Reply::error(500, e),
        };
        let doc = json!({
            "id": id,
            "kind": "card",
            "label": label,
            "brand": brand,
            "last4": last4,
            "expiryMonth": month,
            "expiryYear": year,
            "cardholder": cardholder,
            "cardNumberEnc": enc_num,
            "updatedAt": now_iso,
        });
        if let Err(err) = ctx.store.put("vault_entries", &id, &doc) {
            return Reply::error(500, err);
        }
        record_activity(ctx.store, "card", &label, &id, "card", "Saved payment card in Vault");
        notify_vault(ctx);
        return Reply::ok(entry_public_view(&doc));
    }

    let origin = normalize_origin(&text(&ctx.body, "origin"));
    let username = text(&ctx.body, "username").trim().to_string();
    let password = text(&ctx.body, "password");
    let auth_key = text(&ctx.body, "authenticatorKey").trim().to_string();
    if origin.is_empty() {
        return Reply::error(400, "Enter a website or app address.");
    }
    if username.is_empty() && password.is_empty() {
        return Reply::error(400, "Add a username, email, or password.");
    }
    let label = {
        let l = text(&ctx.body, "label").trim().to_string();
        if l.is_empty() { host_label(&origin) } else { l }
    };
    let password_enc = match encrypt_secret(&password) {
        Ok(v) => v,
        Err(e) => return Reply::error(500, e),
    };
    let auth_enc = match encrypt_secret(&auth_key) {
        Ok(v) => v,
        Err(e) => return Reply::error(500, e),
    };
    let doc = json!({
        "id": id,
        "kind": "login",
        "label": label,
        "origin": origin,
        "username": username,
        "passwordEnc": password_enc,
        "authenticatorEnc": auth_enc,
        "updatedAt": now_iso,
    });
    if let Err(err) = ctx.store.put("vault_entries", &id, &doc) {
        return Reply::error(500, err);
    }
    record_activity(ctx.store, "sign_in", &origin, &id, "login", "Saved login in Vault");
    notify_vault(ctx);
    Reply::ok(entry_public_view(&doc))
}

fn entry_details(ctx: &Ctx, id: &str) -> Reply {
    let Some(entry) = ctx.store.get("vault_entries", id) else {
        return Reply::error(404, "Vault item not found.");
    };
    Reply::ok(entry_details_view(&entry))
}

fn update_entry(ctx: &Ctx, id: &str) -> Reply {
    let Some(mut entry) = ctx.store.get("vault_entries", id) else {
        return Reply::error(404, "Vault item not found.");
    };
    let kind = text(&entry, "kind");
    let now_iso = iso_now();
    let next_label = text(&ctx.body, "label").trim().to_string();

    if kind == "card" {
        if let Some(obj) = entry.as_object_mut() {
            if !next_label.is_empty() {
                obj.insert("label".to_string(), json!(next_label));
            }
            if let Some(holder) = ctx.body.get("cardholder").and_then(Value::as_str) {
                obj.insert("cardholder".to_string(), json!(holder.trim()));
            }
            if let Some(month) = ctx.body.get("expiryMonth").and_then(Value::as_u64) {
                obj.insert("expiryMonth".to_string(), json!(month));
            }
            if let Some(year) = ctx.body.get("expiryYear").and_then(Value::as_u64) {
                obj.insert("expiryYear".to_string(), json!(year));
            }
            let raw_num = text(&ctx.body, "cardNumber");
            if !raw_num.trim().is_empty() {
                let digits: String = raw_num.chars().filter(|c| c.is_ascii_digit()).collect();
                if digits.len() < 12 || digits.len() > 19 {
                    return Reply::error(400, "Enter a valid card number.");
                }
                let enc = match encrypt_secret(&digits) {
                    Ok(v) => v,
                    Err(e) => return Reply::error(500, e),
                };
                obj.insert("cardNumberEnc".to_string(), json!(enc));
                obj.insert("brand".to_string(), json!(detect_card_brand(&digits)));
                obj.insert(
                    "last4".to_string(),
                    json!(&digits[digits.len().saturating_sub(4)..]),
                );
            }
            obj.insert("updatedAt".to_string(), json!(now_iso));
        }
    } else if let Some(obj) = entry.as_object_mut() {
        if !next_label.is_empty() {
            obj.insert("label".to_string(), json!(next_label));
        }
        if let Some(username) = ctx.body.get("username").and_then(Value::as_str) {
            obj.insert("username".to_string(), json!(username.trim()));
        }
        if let Some(pw) = ctx.body.get("password").and_then(Value::as_str)
            && !pw.is_empty()
        {
            let enc = match encrypt_secret(pw) {
                Ok(v) => v,
                Err(e) => return Reply::error(500, e),
            };
            obj.insert("passwordEnc".to_string(), json!(enc));
        }
        if let Some(auth) = ctx.body.get("authenticatorKey").and_then(Value::as_str) {
            if auth.is_empty() {
                obj.insert("authenticatorEnc".to_string(), json!(""));
            } else {
                let enc = match encrypt_secret(auth.trim()) {
                    Ok(v) => v,
                    Err(e) => return Reply::error(500, e),
                };
                obj.insert("authenticatorEnc".to_string(), json!(enc));
            }
        }
        obj.insert("updatedAt".to_string(), json!(now_iso));
    }

    if let Err(err) = ctx.store.put("vault_entries", id, &entry) {
        return Reply::error(500, err);
    }
    notify_vault(ctx);
    Reply::ok(entry_public_view(&entry))
}

fn delete_entry(ctx: &Ctx, id: &str) -> Reply {
    if ctx.store.get("vault_entries", id).is_none() {
        return Reply::error(404, "Vault item not found.");
    }
    ctx.store.delete("vault_entries", id);
    notify_vault(ctx);
    Reply::ok(json!({ "ok": true }))
}

fn reveal_secret(ctx: &Ctx, id: &str, card_number: bool) -> Reply {
    let Some(entry) = ctx.store.get("vault_entries", id) else {
        return Reply::error(404, "Vault item not found.");
    };
    let field = if card_number { "cardNumberEnc" } else { "passwordEnc" };
    let enc = text(&entry, field);
    let plain = match decrypt_secret(&enc) {
        Ok(v) => v,
        Err(e) => return Reply::error(500, e),
    };
    let origin = if card_number { text(&entry, "label") } else { text(&entry, "origin") };
    let kind = if card_number { "card" } else { "login" };
    record_activity(ctx.store, "reveal", &origin, id, kind, "Viewed saved secret");
    if card_number {
        Reply::ok(json!({ "cardNumber": plain }))
    } else {
        Reply::ok(json!({ "password": plain }))
    }
}

fn entry_icon(ctx: &Ctx, id: &str) -> Reply {
    let Some(entry) = ctx.store.get("vault_entries", id) else {
        return Reply::error(404, "Vault item not found.");
    };
    let label = text(&entry, "label");
    let ch = label
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('N');
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 32 32\"><rect width=\"32\" height=\"32\" rx=\"8\" fill=\"#27272a\"/><text x=\"16\" y=\"21\" text-anchor=\"middle\" font-family=\"system-ui,sans-serif\" font-size=\"15\" font-weight=\"600\" fill=\"#fafafa\">{ch}</text></svg>"
    );
    Reply::bytes(200, svg.into_bytes(), "image/svg+xml".to_string())
}

fn import_logins(ctx: &Ctx) -> Reply {
    let Some(items) = ctx.body.get("logins").and_then(Value::as_array) else {
        return Reply::error(400, "Provide logins to import.");
    };
    let mut existing_keys: HashSet<(String, String)> = ctx
        .store
        .all("vault_entries")
        .into_iter()
        .filter(|e| text(e, "kind") != "card")
        .map(|e| (text(&e, "origin").to_ascii_lowercase(), text(&e, "username").to_ascii_lowercase()))
        .collect();

    let mut imported = 0_u64;
    let mut existing = 0_u64;
    let mut skipped = 0_u64;
    let mut authenticator_keys = 0_u64;

    for item in items {
        let origin = normalize_origin(&text(item, "origin"));
        let username = text(item, "username").trim().to_string();
        let password = text(item, "password");
        let auth_key = text(item, "authenticatorKey").trim().to_string();
        if origin.is_empty() || (username.is_empty() && password.is_empty()) {
            skipped += 1;
            continue;
        }
        let dedup = (origin.to_ascii_lowercase(), username.to_ascii_lowercase());
        if !existing_keys.insert(dedup) {
            existing += 1;
            continue;
        }
        let Ok(password_enc) = encrypt_secret(&password) else {
            skipped += 1;
            continue;
        };
        let Ok(auth_enc) = encrypt_secret(&auth_key) else {
            skipped += 1;
            continue;
        };
        if !auth_key.is_empty() {
            authenticator_keys += 1;
        }
        let label = {
            let raw = text(item, "label").trim().to_string();
            if raw.is_empty() { host_label(&origin) } else { raw }
        };
        let id = uuid();
        let doc = json!({
            "id": id,
            "kind": "login",
            "label": label,
            "origin": origin,
            "username": username,
            "passwordEnc": password_enc,
            "authenticatorEnc": auth_enc,
            "updatedAt": iso_now(),
        });
        if ctx.store.put("vault_entries", &id, &doc).is_ok() {
            imported += 1;
        } else {
            skipped += 1;
        }
    }

    if imported > 0 {
        record_activity(
            ctx.store,
            "sign_in",
            "vault-import",
            "import",
            "login",
            &format!("Imported {imported} logins"),
        );
        notify_vault(ctx);
    }

    Reply::ok(json!({
        "imported": imported,
        "existing": existing,
        "skipped": skipped,
        "authenticatorKeys": authenticator_keys,
    }))
}

fn list_sessions(ctx: &Ctx) -> Reply {
    Reply::ok(Value::Array(ctx.store.all("vault_sessions")))
}

fn delete_session(ctx: &Ctx, id: &str) -> Reply {
    ctx.store.delete("vault_sessions", id);
    notify_vault(ctx);
    Reply::ok(json!({ "ok": true }))
}

fn list_activity(ctx: &Ctx) -> Reply {
    let filter_item = ctx.param("item").unwrap_or_default();
    let filter_origin = ctx.param("origin").unwrap_or_default().to_ascii_lowercase();
    let cutoff = (now() - Duration::days(7)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    let mut entries: Vec<Value> = ctx.store.all("vault_activity");
    entries.sort_by_key(|e| std::cmp::Reverse(text(e, "at")));

    let mut sign_ins = 0_u64;
    let mut payments = 0_u64;
    let mut held = 0_u64;
    for entry in &entries {
        if text(entry, "at") < cutoff {
            continue;
        }
        match text(entry, "action").as_str() {
            "sign_in" | "session" | "code" => sign_ins += 1,
            "card" => payments += 1,
            "held" | "asked" => held += 1,
            _ => {}
        }
    }

    let filtered: Vec<Value> = entries
        .into_iter()
        .filter(|e| filter_item.is_empty() || text(e, "itemId") == filter_item)
        .filter(|e| filter_origin.is_empty() || text(e, "origin").to_ascii_lowercase().contains(&filter_origin))
        .take(50)
        .collect();

    Reply::ok(json!({
        "week": {
            "signIns": sign_ins,
            "payments": payments,
            "held": held,
        },
        "entries": filtered,
        "next": Value::Null,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeManager;
    use crate::server::Live;
    use std::collections::HashMap;
    use std::sync::Arc;

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

    #[test]
    fn vault_dpapi_crud_reveal_and_import_work() {
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let live = Arc::new(Live::default());
        let runtime = RuntimeManager::new(Arc::clone(&live));

        let created = call(
            &store,
            &live,
            &runtime,
            "POST",
            "/api/vault",
            json!({
                "label": "GitHub",
                "origin": "github.com",
                "username": "amr@example.com",
                "password": "super-secret-password"
            }),
        );
        assert_eq!(created.status, 200);
        assert_eq!(created.body["origin"], "https://github.com");
        let id = created.body["id"].as_str().unwrap().to_string();

        // Password never leaks in list or details
        let list = call(&store, &live, &runtime, "GET", "/api/vault", Value::Null);
        assert!(!list.body.to_string().contains("super-secret-password"));

        let revealed = call(&store, &live, &runtime, "POST", &format!("/api/vault/{id}/password"), json!({}));
        assert_eq!(revealed.body["password"], "super-secret-password");

        let imported = call(
            &store,
            &live,
            &runtime,
            "POST",
            "/api/vault/import",
            json!({
                "logins": [
                    { "origin": "github.com", "username": "amr@example.com", "password": "dup" },
                    { "origin": "gitlab.com", "username": "amr2", "password": "pw2", "authenticatorKey": "JBSWY3DPEHPK3PXP" }
                ]
            }),
        );
        assert_eq!(imported.body["imported"], 1);
        assert_eq!(imported.body["existing"], 1);
        assert_eq!(imported.body["authenticatorKeys"], 1);
    }
}
