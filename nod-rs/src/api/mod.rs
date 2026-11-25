pub mod account;
pub mod apps;
pub mod bots;
pub mod browser;
pub mod channels;
pub mod chat;
pub mod computer_use;
pub mod jobs;
pub mod local_files;
pub mod memory;
pub mod misc;
pub mod pages;
pub mod schedules;
pub mod tasks;
pub mod usage;
pub mod vault;

use crate::db::Store;
use crate::runtime::RuntimeManager;
use crate::server::Live;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub struct Ctx<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a HashMap<String, String>,
    pub body: Value,
    #[allow(dead_code)]
    pub body_bytes: &'a [u8],
    #[allow(dead_code)]
    pub content_type: &'a str,
    pub store: &'a Arc<Store>,
    pub live: &'a Arc<Live>,
    pub runtime: &'a Arc<RuntimeManager>,
    pub owner: &'a str,
}

pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub raw: Option<(Vec<u8>, String)>,
    pub location: Option<String>,
}

impl Reply {
    pub fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            body,
            raw: None,
            location: None,
        }
    }

    pub fn ok(body: Value) -> Self {
        Self::json(200, body)
    }

    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, json!({ "error": message.into() }))
    }

    pub fn text(status: u16, body: String, content_type: &'static str) -> Self {
        Self {
            status,
            body: Value::Null,
            raw: Some((body.into_bytes(), content_type.to_string())),
            location: None,
        }
    }

    pub fn bytes(status: u16, body: Vec<u8>, content_type: String) -> Self {
        Self {
            status,
            body: Value::Null,
            raw: Some((body, content_type)),
            location: None,
        }
    }

    pub fn redirect(location: impl Into<String>) -> Self {
        Self {
            status: 302,
            body: Value::Null,
            raw: Some((Vec::new(), "text/html; charset=utf-8".to_string())),
            location: Some(location.into()),
        }
    }
}

impl Ctx<'_> {
    pub fn refresh(&self) {
        self.live.broadcast(&json!({ "type": "refresh" }));
    }

    pub fn param(&self, name: &str) -> Option<String> {
        self.query.get(name).cloned()
    }
}

pub fn dispatch(ctx: &Ctx) -> Reply {
    if ctx.method == "GET" && ctx.path == "/api/desktop/status" {
        return Reply::ok(ctx.runtime.status_json());
    }
    account::handle(ctx)
        .or_else(|| bots::handle(ctx))
        .or_else(|| chat::handle(ctx))
        .or_else(|| browser::handle(ctx))
        .or_else(|| computer_use::handle(ctx))
        .or_else(|| local_files::handle(ctx))
        .or_else(|| schedules::handle(ctx))
        .or_else(|| tasks::handle(ctx))
        .or_else(|| usage::handle(ctx))
        .or_else(|| memory::handle(ctx))
        .or_else(|| jobs::handle(ctx))
        .or_else(|| pages::handle(ctx))
        .or_else(|| vault::handle(ctx))
        .or_else(|| channels::handle(ctx))
        .or_else(|| apps::handle(ctx))
        .or_else(|| misc::handle(ctx))
        .unwrap_or_else(|| fallback(ctx))
}

pub fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub fn flag(value: &Value, key: &str, fallback: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(fallback)
}

#[allow(dead_code)]
pub fn number(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

pub fn provider_name(provider: &str) -> String {
    match provider {
        "openai" => "OpenAI",
        "anthropic" => "Anthropic",
        "google" => "Google",
        "openrouter" => "OpenRouter",
        "hostinger" => "Hostinger AI Router",
        "ollama" => "Ollama",
        "compatible" => "OpenAI-compatible server",
        other => other,
    }
    .to_string()
}

pub fn session_state(store: &Store) -> Value {
    store.setting("session").unwrap_or_else(default_session)
}

fn default_session() -> Value {
    let name = std::env::var("NOD_OWNER_NAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("USERNAME").ok())
        .unwrap_or_else(|| "You".to_string());
    json!({
        "name": name,
        "email": Value::Null,
        "avatarUrl": Value::Null,
        "theme": "dark",
        "onboardingCompleted": true,
        "usageData": true,
        "traces": false
    })
}

fn optional_text(value: &Value, key: &str) -> Value {
    match value.get(key).and_then(Value::as_str) {
        Some(text) if !text.is_empty() => json!(text),
        _ => Value::Null,
    }
}

pub fn session_view(store: &Store) -> Value {
    let session = session_state(store);
    let theme = text(&session, "theme");
    let theme = if matches!(
        theme.as_str(),
        "system" | "light" | "dark" | "amoled" | "catppuccin-mocha"
    ) {
        theme
    } else {
        "dark".to_string()
    };
    json!({
        "user": {
            "id": OWNER,
            "name": text(&session, "name"),
            "email": optional_text(&session, "email"),
            "avatarUrl": optional_text(&session, "avatarUrl")
        },
        "settings": {
            "theme": theme,
            "onboardingCompleted": flag(&session, "onboardingCompleted", true),
            "usageData": flag(&session, "usageData", true),
            "traces": flag(&session, "traces", false),
            "usageDataRevision": "1"
        },
        "capabilities": {
            "developer": std::env::var_os("NOD_DEVTOOLS").is_some(),
            "traces": flag(&session, "traces", false),
            "updatesEmail": false
        }
    })
}

pub fn preferences_state(store: &Store) -> Value {
    let mut preferences = store.setting("preferences").unwrap_or_else(|| {
        json!({
            "speechVoice": Value::Null,
            "speechReply": "auto",
            "notificationRule": "",
            "rules": "",
            "timeZone": crate::util::local_zone().name(),
            "locale": "en-US",
            "hourCycle": "auto",
            "quietHoursEnabled": false,
            "quietStart": "22:00",
            "quietEnd": "07:00",
            "proactivePaused": false,
            "localFilesEnabled": true
        })
    });
    if preferences.get("timeZone").and_then(Value::as_str).is_none() {
        preferences["timeZone"] = json!(crate::util::local_zone().name());
    }
    preferences
}

pub fn models_state(store: &Store) -> Value {
    store.setting("models").unwrap_or_else(empty_models)
}

pub const OWNER: &str = "d0000001-0000-4000-8000-000000000001";
pub const DEMO_CONNECTION: &str = "m0000001-0000-4000-8000-000000000001";

pub fn demo_connection() -> Value {
    json!({
        "id": DEMO_CONNECTION,
        "provider": "compatible",
        "method": "api_key",
        "label": "Nod demo engine",
        "status": "connected",
        "models": [{
            "id": "nod-demo",
            "name": "Nod Demo",
            "efforts": ["off"],
            "images": false,
            "contextWindow": 128000
        }],
        "managed": false
    })
}

pub fn demo_selection() -> Value {
    json!({
        "connectionId": DEMO_CONNECTION,
        "modelId": "nod-demo",
        "effort": "off"
    })
}

pub fn empty_models() -> Value {
    json!({
        "connections": [],
        "selection": Value::Null,
        "routes": {},
        "backups": [],
        "light": Value::Null
    })
}

fn resolved_models(store: &Store) -> Value {
    let mut state = models_state(store);
    let mut connections = state
        .get("connections")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !connections.iter().any(|connection| {
        connection.get("id").and_then(Value::as_str) == Some(DEMO_CONNECTION)
    }) {
        connections.push(demo_connection());
    }
    state["connections"] = Value::Array(connections);
    if state.get("selection").map(Value::is_null).unwrap_or(true) {
        state["selection"] = demo_selection();
    }
    if !state.get("routes").map(Value::is_object).unwrap_or(false) {
        state["routes"] = json!({});
    }
    state
}

fn redact_models(state: &mut Value) {
    let Some(connections) = state.get_mut("connections").and_then(Value::as_array_mut) else {
        return;
    };
    for connection in connections {
        let Some(object) = connection.as_object_mut() else {
            continue;
        };
        object.remove("key");
        object.remove("apiKey");
    }
}

pub fn models_view(store: &Store) -> Value {
    let mut state = resolved_models(store);
    redact_models(&mut state);
    state
}

pub fn models_internal(store: &Store) -> Value {
    resolved_models(store)
}

/// Resolves the connection and model metadata for either an explicit selection or global selection.
pub fn selected_model_override(store: &Store, override_sel: Option<&Value>) -> Option<(Value, Value)> {
    let state = models_internal(store);
    let selection = override_sel
        .filter(|v| !v.is_null())
        .or_else(|| state.get("selection"))?;
    let connection_id = selection.get("connectionId")?.as_str()?;
    let model_id = selection.get("modelId")?.as_str()?;
    let connections = state.get("connections")?.as_array()?;
    let connection = connections
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(connection_id))?
        .clone();
    let model = connection
        .get("models")?
        .as_array()?
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(model_id))?
        .clone();
    Some((connection, model))
}

pub fn configuration(store: &Store, runtime_ready: bool) -> Value {
    configuration_for_bot(store, runtime_ready, None)
}

/// Returns effective model/runtime configuration, honoring a bot's custom model & backups if set.
pub fn configuration_for_bot(store: &Store, runtime_ready: bool, bot_id: Option<&str>) -> Value {
    let (model_ov, backups_ov) = bot_id
        .and_then(|id| bots::get_bot(store, id))
        .map(|bot| bots::bot_model_override(&bot))
        .unwrap_or((None, None));
    let Some((connection, model)) = selected_model_override(store, model_ov.as_ref()) else {
        return json!({
            "provider": "compatible",
            "model": "Nod Demo",
            "llmReady": false,
            "integrationsReady": runtime_ready,
            "modelReconnect": false,
            "modelIssue": { "reason": "Choose a model in Settings." },
            "conversationResetEnabled": true
        });
    };
    let status = connection
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("reconnect");
    let mut value = json!({
        "provider": text(&connection, "provider"),
        "model": text(&model, "name"),
        "llmReady": status == "connected",
        "integrationsReady": runtime_ready,
        "modelReconnect": status != "connected",
        "conversationResetEnabled": true
    });
    if status != "connected" {
        value["modelIssue"] = json!({
            "reason": "Reconnect the model in Settings to keep chatting."
        });
        let state = models_internal(store);
        if let Some(alt) = first_connected_backup(&state, backups_ov.as_deref()) {
            value["modelAlternative"] = alt;
        }
    }
    value
}

fn first_connected_backup(state: &Value, backups_override: Option<&[Value]>) -> Option<Value> {
    let connections = state.get("connections")?.as_array()?;
    let owned: Vec<Value>;
    let backups: &[Value] = match backups_override {
        Some(items) => items,
        None => {
            owned = state.get("backups")?.as_array()?.clone();
            &owned
        }
    };
    for item in backups {
        let conn_id = item.get("connectionId")?.as_str()?;
        let model_id = item.get("modelId")?.as_str()?;
        let effort = item.get("effort").and_then(Value::as_str).unwrap_or("high");
        let conn = connections
            .iter()
            .find(|c| c.get("id").and_then(Value::as_str) == Some(conn_id))?;
        if conn.get("status").and_then(Value::as_str) != Some("connected") {
            continue;
        }
        let model_name = conn
            .get("models")?
            .as_array()?
            .iter()
            .find(|m| m.get("id").and_then(Value::as_str) == Some(model_id))
            .and_then(|m| m.get("name").and_then(Value::as_str))
            .unwrap_or(model_id);
        return Some(json!({
            "connectionId": conn_id,
            "modelId": model_id,
            "effort": effort,
            "label": model_name
        }));
    }
    None
}

pub fn seed(store: &Store) {
    if store.setting("session").is_none() {
        store.set_setting("session", &default_session());
    }
    if store.setting("preferences").is_none() {
        let defaults = preferences_state(store);
        store.set_setting("preferences", &defaults);
    }
    if store.setting("memory_settings").is_none() {
        store.set_setting("memory_settings", &json!({ "saving": true, "sensitive": false }));
    }
    if store.setting("models").is_none() {
        store.set_setting("models", &empty_models());
    }
    if store.setting("thread").is_none() {
        chat::new_thread(store);
    }
    bots::seed(store);
}

fn fallback(_ctx: &Ctx) -> Reply {
    Reply::error(404, "Unknown API endpoint.")
}
