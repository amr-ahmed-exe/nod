//! Telegram and WhatsApp messaging channel configuration, pairing, and live bot routing.

use super::{bots, text, vault, Ctx, Reply};
use crate::db::Store;
use crate::runtime::storage;
use crate::server::Live;
use crate::util::now;
use chrono::Duration;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration as StdDuration;

static TELEGRAM_POLLER_RUNNING: OnceLock<AtomicBool> = OnceLock::new();

fn poller_flag() -> &'static AtomicBool {
    TELEGRAM_POLLER_RUNNING.get_or_init(|| AtomicBool::new(false))
}

/// Ensures the background Telegram polling worker starts once if Telegram is enabled.
pub fn ensure_background_listeners(store: Arc<Store>, live: Arc<Live>) {
    let settings = store.setting("channel_telegram").unwrap_or(Value::Null);
    let enabled = settings.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    let mode = text(&settings, "mode");
    if enabled && mode != "webhook" {
        spawn_telegram_poller(store, live);
    }
}

/// Handles `/api/channels*`, `/api/channel-settings*`, and `/api/webhooks/whatsapp`.
pub fn handle(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    match (method, path) {
        ("GET", "/api/channels") => Some(channels_status(ctx)),
        ("GET", "/api/channel-settings") => Some(channel_settings(ctx)),
        ("PUT", "/api/channel-settings/telegram") => Some(save_telegram(ctx)),
        ("DELETE", "/api/channel-settings/telegram") => Some(disable_channel(ctx, "telegram")),
        ("POST", "/api/channel-settings/telegram/pairing") => Some(restart_telegram_pairing(ctx)),
        ("POST", "/api/channel-settings/telegram/confirm") => Some(confirm_telegram(ctx, None)),
        ("POST", "/api/channel-settings/telegram/user") => {
            let id = text(&ctx.body, "allowedUserId").trim().to_string();
            Some(confirm_telegram(ctx, Some(id)))
        }
        ("POST", "/api/channel-settings/telegram/reconnect") => Some(reconnect_channel(ctx, "telegram")),
        ("PUT", "/api/channel-settings/whatsapp") => Some(save_whatsapp(ctx)),
        ("DELETE", "/api/channel-settings/whatsapp") => Some(disable_channel(ctx, "whatsapp")),
        ("POST", "/api/channel-settings/whatsapp/reconnect") => Some(reconnect_channel(ctx, "whatsapp")),
        ("GET", "/api/webhooks/whatsapp") => Some(whatsapp_webhook_verify(ctx)),
        ("POST", "/api/webhooks/whatsapp") => Some(Reply::ok(json!({ "ok": true }))),
        _ => None,
    }
}

fn telegram_summary(doc: Option<&Value>) -> Value {
    let Some(cfg) = doc else {
        return json!({ "provider": "telegram", "enabled": false });
    };
    json!({
        "provider": "telegram",
        "enabled": cfg.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        "accountName": cfg.get("accountName").cloned().unwrap_or(Value::Null),
        "accountHandle": cfg.get("accountHandle").cloned().unwrap_or(Value::Null),
        "mode": cfg.get("mode").cloned().unwrap_or(json!("polling")),
        "webhookStatus": cfg.get("webhookStatus").cloned().unwrap_or(Value::Null),
        "webhookUrl": cfg.get("webhookUrl").cloned().unwrap_or(Value::Null),
        "allowedUserId": cfg.get("allowedUserId").cloned().unwrap_or(Value::Null),
        "pairedUser": cfg.get("pairedUser").cloned().unwrap_or(Value::Null),
        "pairedUsername": cfg.get("pairedUsername").cloned().unwrap_or(Value::Null),
        "pairingCandidate": cfg.get("pairingCandidate").cloned().unwrap_or(Value::Null),
        "pairingExpiresAt": cfg.get("pairingExpiresAt").cloned().unwrap_or(Value::Null),
    })
}

fn whatsapp_summary(doc: Option<&Value>) -> Value {
    let Some(cfg) = doc else {
        return json!({ "provider": "whatsapp", "enabled": false });
    };
    json!({
        "provider": "whatsapp",
        "enabled": cfg.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        "accountName": cfg.get("accountName").cloned().unwrap_or(Value::Null),
        "accountHandle": cfg.get("accountHandle").cloned().unwrap_or(Value::Null),
        "mode": cfg.get("mode").cloned().unwrap_or(json!("webhook")),
        "connectionState": cfg.get("connectionState").cloned().unwrap_or(Value::Null),
        "webhookStatus": cfg.get("webhookStatus").cloned().unwrap_or(Value::Null),
        "webhookUrl": cfg.get("webhookUrl").cloned().unwrap_or(Value::Null),
    })
}

fn channels_status(ctx: &Ctx) -> Reply {
    let tg = ctx.store.setting("channel_telegram");
    let wa = ctx.store.setting("channel_whatsapp");

    let tg_enabled = tg.as_ref().and_then(|v| v.get("enabled")).and_then(Value::as_bool).unwrap_or(false);
    let wa_enabled = wa.as_ref().and_then(|v| v.get("enabled")).and_then(Value::as_bool).unwrap_or(false);

    Reply::ok(json!({
        "channels": [
            {
                "provider": "telegram",
                "status": if tg_enabled { "configured" } else { "disabled" },
                "missing": if tg_enabled { json!([]) } else { json!(["botToken"]) },
                "available": tg_enabled
            },
            {
                "provider": "whatsapp",
                "status": if wa_enabled { "configured" } else { "disabled" },
                "missing": if wa_enabled { json!([]) } else { json!(["credentials"]) },
                "available": wa_enabled
            }
        ],
        "error": Value::Null
    }))
}

fn channel_settings(ctx: &Ctx) -> Reply {
    let tg = ctx.store.setting("channel_telegram");
    let wa = ctx.store.setting("channel_whatsapp");
    Reply::ok(json!({
        "ready": true,
        "channels": [
            telegram_summary(tg.as_ref()),
            whatsapp_summary(wa.as_ref())
        ]
    }))
}

fn telegram_get_me(bot_token: &str) -> Result<(String, String), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(StdDuration::from_secs(6))
        .timeout(StdDuration::from_secs(10))
        .build();
    let url = format!("https://api.telegram.org/bot{}/getMe", bot_token.trim());
    let resp = agent
        .get(&url)
        .call()
        .map_err(|_| "Could not verify the bot token with Telegram. Check the token and your network.".to_string())?;
    let val: Value = serde_json::from_str(&resp.into_string().map_err(|e| e.to_string())?)
        .map_err(|_| "Unexpected response from Telegram.".to_string())?;
    if !val.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return Err("Telegram rejected that bot token.".to_string());
    }
    let name = val
        .pointer("/result/first_name")
        .and_then(Value::as_str)
        .unwrap_or("Telegram Bot")
        .to_string();
    let username = val
        .pointer("/result/username")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let handle = if username.is_empty() {
        String::new()
    } else {
        format!("@{username}")
    };
    Ok((name, handle))
}

fn telegram_send_message(bot_token: &str, chat_id: i64, message: &str) {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(StdDuration::from_secs(6))
        .timeout(StdDuration::from_secs(12))
        .build();
    let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");
    let _ = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .send_string(
            &json!({
                "chat_id": chat_id,
                "text": message
            })
            .to_string(),
        );
}

fn save_telegram(ctx: &Ctx) -> Reply {
    let bot_token = text(&ctx.body, "botToken").trim().to_string();
    let webhook_url = text(&ctx.body, "webhookUrl").trim().to_string();
    if bot_token.is_empty() {
        return Reply::error(400, "Enter your Telegram bot token.");
    }
    let (account_name, account_handle) = match telegram_get_me(&bot_token) {
        Ok(pair) => pair,
        Err(err) => return Reply::error(400, err),
    };
    let enc_token = match vault::encrypt_secret(&bot_token) {
        Ok(v) => v,
        Err(e) => return Reply::error(500, e),
    };

    let pairing_code = storage::random_hex(16); // exactly 32 alphanumeric hex characters
    let expires_at = (now() + Duration::minutes(15)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mode = if webhook_url.is_empty() { "polling" } else { "webhook" };
    let doc = json!({
        "enabled": true,
        "tokenEnc": enc_token,
        "accountName": account_name,
        "accountHandle": account_handle,
        "mode": mode,
        "webhookStatus": if webhook_url.is_empty() { Value::Null } else { json!("registered") },
        "webhookUrl": if webhook_url.is_empty() { Value::Null } else { json!(webhook_url) },
        "pairingCode": pairing_code,
        "pairingExpiresAt": expires_at,
        "pairingCandidate": Value::Null,
        "allowedUserId": Value::Null,
        "pairedUser": Value::Null,
        "pairedUsername": Value::Null,
        "lastUpdateId": 0,
    });
    ctx.store.set_setting("channel_telegram", &doc);
    if mode == "polling" {
        spawn_telegram_poller(Arc::clone(ctx.store), Arc::clone(ctx.live));
    }
    ctx.live.notify("channels");
    ctx.refresh();
    let pairing_url = format!(
        "https://t.me/{}?start={}",
        account_handle.trim_start_matches('@'),
        pairing_code
    );
    Reply::ok(json!({
        "channel": telegram_summary(Some(&doc)),
        "pairingUrl": pairing_url
    }))
}

fn restart_telegram_pairing(ctx: &Ctx) -> Reply {
    let Some(mut doc) = ctx.store.setting("channel_telegram") else {
        return Reply::error(404, "Connect your Telegram bot token first.");
    };
    let pairing_code = storage::random_hex(16);
    let expires_at = (now() + Duration::minutes(15)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("pairingCode".to_string(), json!(pairing_code));
        obj.insert("pairingExpiresAt".to_string(), json!(expires_at));
        obj.insert("pairingCandidate".to_string(), Value::Null);
    }
    ctx.store.set_setting("channel_telegram", &doc);
    spawn_telegram_poller(Arc::clone(ctx.store), Arc::clone(ctx.live));
    ctx.live.notify("channels");
    let handle = text(&doc, "accountHandle");
    let pairing_url = format!(
        "https://t.me/{}?start={}",
        handle.trim_start_matches('@'),
        pairing_code
    );
    Reply::ok(json!({
        "channel": telegram_summary(Some(&doc)),
        "pairingUrl": pairing_url
    }))
}

fn confirm_telegram(ctx: &Ctx, explicit_id: Option<String>) -> Reply {
    let Some(mut doc) = ctx.store.setting("channel_telegram") else {
        return Reply::error(404, "Telegram is not connected.");
    };
    let (uid, uname, uhandle) = if let Some(id) = explicit_id {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
            return Reply::error(400, "Enter a valid numeric Telegram user ID.");
        }
        (id, "Telegram User".to_string(), String::new())
    } else {
        let Some(cand) = doc.get("pairingCandidate").filter(|v| !v.is_null()) else {
            return Reply::error(400, "Send the Start command to your bot in Telegram first.");
        };
        (
            text(cand, "id"),
            text(cand, "name"),
            text(cand, "username"),
        )
    };
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("allowedUserId".to_string(), json!(uid));
        obj.insert("pairedUser".to_string(), json!(uname));
        obj.insert("pairedUsername".to_string(), if uhandle.is_empty() { Value::Null } else { json!(uhandle) });
        obj.insert("pairingExpiresAt".to_string(), Value::Null);
        obj.insert("pairingCandidate".to_string(), Value::Null);
    }
    ctx.store.set_setting("channel_telegram", &doc);
    ctx.live.notify("channels");
    ctx.refresh();
    Reply::ok(json!({ "channel": telegram_summary(Some(&doc)) }))
}

fn save_whatsapp(ctx: &Ctx) -> Reply {
    let access_token = text(&ctx.body, "accessToken").trim().to_string();
    let app_secret = text(&ctx.body, "appSecret").trim().to_string();
    let phone_id = text(&ctx.body, "phoneNumberId").trim().to_string();
    let allowed = text(&ctx.body, "allowedNumber").trim().to_string();
    let verify = text(&ctx.body, "verifyToken").trim().to_string();
    let webhook = text(&ctx.body, "webhookUrl").trim().to_string();
    if access_token.is_empty() || app_secret.is_empty() || phone_id.is_empty() || allowed.is_empty() || verify.is_empty() {
        return Reply::error(400, "Complete all required WhatsApp Cloud API credentials.");
    }
    let enc_access = match vault::encrypt_secret(&access_token) {
        Ok(v) => v,
        Err(e) => return Reply::error(500, e),
    };
    let enc_secret = match vault::encrypt_secret(&app_secret) {
        Ok(v) => v,
        Err(e) => return Reply::error(500, e),
    };
    let enc_verify = match vault::encrypt_secret(&verify) {
        Ok(v) => v,
        Err(e) => return Reply::error(500, e),
    };
    let doc = json!({
        "enabled": true,
        "accountName": format!("WhatsApp +{allowed}"),
        "accountHandle": phone_id,
        "mode": "webhook",
        "connectionState": "connected",
        "webhookStatus": if webhook.is_empty() { "manual_setup" } else { "registered" },
        "webhookUrl": if webhook.is_empty() { Value::Null } else { json!(webhook) },
        "phoneNumberId": phone_id,
        "allowedNumber": allowed,
        "accessTokenEnc": enc_access,
        "appSecretEnc": enc_secret,
        "verifyTokenEnc": enc_verify,
    });
    ctx.store.set_setting("channel_whatsapp", &doc);
    ctx.live.notify("channels");
    ctx.refresh();
    Reply::ok(json!({ "channel": whatsapp_summary(Some(&doc)) }))
}

fn disable_channel(ctx: &Ctx, provider: &str) -> Reply {
    let key = format!("channel_{provider}");
    ctx.store.set_setting(&key, &json!({ "enabled": false }));
    ctx.live.notify("channels");
    ctx.refresh();
    let summary = if provider == "telegram" {
        telegram_summary(None)
    } else {
        whatsapp_summary(None)
    };
    Reply::ok(json!({ "channel": summary }))
}

fn reconnect_channel(ctx: &Ctx, provider: &str) -> Reply {
    if provider == "telegram" {
        let Some(doc) = ctx.store.setting("channel_telegram") else {
            return Reply::error(404, "Telegram is not connected.");
        };
        let Ok(token) = vault::decrypt_secret(&text(&doc, "tokenEnc")) else {
            return Reply::error(500, "Could not decrypt stored bot token.");
        };
        if let Err(err) = telegram_get_me(&token) {
            return Reply::error(400, err);
        }
        spawn_telegram_poller(Arc::clone(ctx.store), Arc::clone(ctx.live));
        return Reply::ok(json!({ "channel": telegram_summary(Some(&doc)) }));
    }
    let Some(doc) = ctx.store.setting("channel_whatsapp") else {
        return Reply::error(404, "WhatsApp is not connected.");
    };
    Reply::ok(json!({ "channel": whatsapp_summary(Some(&doc)) }))
}

fn whatsapp_webhook_verify(ctx: &Ctx) -> Reply {
    let mode = ctx.param("hub.mode").unwrap_or_default();
    let token = ctx.param("hub.verify_token").unwrap_or_default();
    let challenge = ctx.param("hub.challenge").unwrap_or_default();
    let Some(doc) = ctx.store.setting("channel_whatsapp") else {
        return Reply::error(403, "WhatsApp not configured.");
    };
    let expected = vault::decrypt_secret(&text(&doc, "verifyTokenEnc")).unwrap_or_default();
    if mode == "subscribe" && !expected.is_empty() && token == expected {
        return Reply::text(200, challenge, "text/plain");
    }
    Reply::error(403, "Verification token mismatch.")
}

fn spawn_telegram_poller(store: Arc<Store>, live: Arc<Live>) {
    if poller_flag().swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(move || {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(StdDuration::from_secs(8))
            .timeout(StdDuration::from_secs(20))
            .build();
        while let Some(mut cfg) = store.setting("channel_telegram") {
            let enabled = cfg.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            let mode = text(&cfg, "mode");
            if !enabled || mode == "webhook" {
                break;
            }
            let Ok(token) = vault::decrypt_secret(&text(&cfg, "tokenEnc")) else {
                break;
            };
            let offset = cfg.get("lastUpdateId").and_then(Value::as_i64).unwrap_or(0);
            let url = format!(
                "https://api.telegram.org/bot{token}/getUpdates?timeout=10&offset={}",
                offset + 1
            );
            let Ok(resp) = agent.get(&url).call() else {
                thread::sleep(StdDuration::from_secs(4));
                continue;
            };
            let Ok(body) = resp.into_string() else {
                thread::sleep(StdDuration::from_secs(2));
                continue;
            };
            let Ok(parsed) = serde_json::from_str::<Value>(&body) else {
                thread::sleep(StdDuration::from_secs(2));
                continue;
            };
            if let Some(updates) = parsed.get("result").and_then(Value::as_array) {
                for upd in updates {
                    let uid = upd.get("update_id").and_then(Value::as_i64).unwrap_or(offset);
                    if let Some(obj) = cfg.as_object_mut() {
                        obj.insert("lastUpdateId".to_string(), json!(uid));
                    }
                    let Some(msg) = upd.get("message") else {
                        continue;
                    };
                    let chat_id = msg.pointer("/chat/id").and_then(Value::as_i64).unwrap_or(0);
                    let from_id = msg.pointer("/from/id").and_then(Value::as_i64).unwrap_or(0).to_string();
                    let from_name = msg
                        .pointer("/from/first_name")
                        .and_then(Value::as_str)
                        .unwrap_or("User")
                        .to_string();
                    let from_user = msg
                        .pointer("/from/username")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let text_msg = msg.get("text").and_then(Value::as_str).unwrap_or("").trim();
                    let pairing_code = text(&cfg, "pairingCode");
                    if !pairing_code.is_empty() && text_msg == format!("/start {pairing_code}") {
                        if let Some(obj) = cfg.as_object_mut() {
                            obj.insert(
                                "pairingCandidate".to_string(),
                                json!({
                                    "id": from_id,
                                    "name": from_name,
                                    "username": from_user
                                }),
                            );
                        }
                        store.set_setting("channel_telegram", &cfg);
                        live.notify("channels");
                        telegram_send_message(&token, chat_id, "Account detected! Return to Nod to confirm.");
                        continue;
                    }
                    let allowed = text(&cfg, "allowedUserId");
                    if !allowed.is_empty() && allowed == from_id && !text_msg.is_empty() {
                        let main_bot = bots::main_bot(store.as_ref());
                        let bot_id = main_bot
                            .as_ref()
                            .map(|b| text(b, "id"))
                            .unwrap_or_else(|| "main".to_string());
                        let sys = bots::build_system_prompt(store.as_ref(), &bot_id, false);
                        let reply_text = match crate::llm::complete_with_selection(
                            store.as_ref(),
                            None,
                            None,
                            Some(&sys),
                            &[("user".to_string(), text_msg.to_string())],
                        ) {
                            Ok(out) => out.text,
                            Err(e) if e == crate::llm::DEMO => crate::llm::demo_reply(text_msg),
                            Err(e) => format!("Could not answer right now: {e}"),
                        };
                        telegram_send_message(&token, chat_id, &reply_text);
                    }
                }
                store.set_setting("channel_telegram", &cfg);
            }
        }
        poller_flag().store(false, Ordering::SeqCst);
    });
}
