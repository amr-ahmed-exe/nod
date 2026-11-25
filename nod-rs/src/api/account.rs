use super::{
    configuration, models_view, preferences_state, session_state, session_view, Ctx, Reply,
};
use crate::runtime::state::Phase;
use crate::util::{local_zone, uuid};
use serde_json::{json, Map, Value};

pub fn handle(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    if method == "GET" && path == "/api/session" {
        return Some(Reply::ok(session_view(ctx.store)));
    }
    if path == "/api/health" && method == "GET" {
        return Some(Reply::ok(json!({
            "ok": true,
            "runtimeReady": ctx.runtime.is_ready(),
            "runtime": ctx.runtime.status_json()["state"]
        })));
    }
    if method == "GET" && path == "/api/account" {
        return Some(Reply::ok(session_view(ctx.store)));
    }
    if method == "PATCH" && path == "/api/account" {
        return Some(account_update(ctx));
    }
    if method == "POST" && path == "/api/account/prepare" {
        let retry = ctx
            .body
            .get("retry")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let status = ctx.runtime.prepare(retry);
        let ready = status.state == Phase::Ready;
        let http = if ready { 200 } else { 503 };
        return Some(Reply::json(
            http,
            json!({
                "ok": ready,
                "ready": ready,
                "status": if ready { Value::String("ready".into()) } else { Value::String("setup_required".into()) },
                "runtime": status.to_json()
            }),
        ));
    }
    if method == "POST" && path == "/api/account/model-setup" {
        return Some(model_setup(ctx));
    }
    if method == "POST" && path == "/api/account/survey-seen" {
        let mut session = session_state(ctx.store);
        if let Some(obj) = session.as_object_mut() {
            obj.insert("surveySeen".to_string(), json!(true));
        }
        ctx.store.set_setting("session", &session);
        return Some(Reply::ok(json!({ "ok": true })));
    }
    if method == "POST" && path == "/api/account/welcome" {
        let mut session = session_state(ctx.store);
        if let Some(obj) = session.as_object_mut() {
            obj.insert("welcomeSeen".to_string(), json!(true));
        }
        ctx.store.set_setting("session", &session);
        return Some(Reply::ok(json!({ "ok": true })));
    }
    if method == "GET" && path == "/api/preferences" {
        return Some(Reply::ok(preferences_state(ctx.store)));
    }
    if method == "PATCH" && path == "/api/preferences" {
        return Some(preferences_update(ctx));
    }
    if method == "GET" && path == "/api/configuration" {
        return Some(Reply::ok(configuration(ctx.store, ctx.runtime.is_ready())));
    }
    if method == "GET" && path == "/api/privacy" {
        return Some(Reply::ok(json!({ "content": Value::Null })));
    }
    if method == "GET" && path == "/api/models" {
        return Some(Reply::ok(models_view(ctx.store)));
    }
    if method == "POST" && path == "/api/models/keys" {
        return Some(model_key(ctx));
    }
    if method == "POST" && path == "/api/models/endpoints" {
        return Some(model_endpoint(ctx));
    }
    if method == "PUT" && path == "/api/models/selection" {
        return Some(model_selection(ctx));
    }
    if method == "PUT" && path == "/api/models/routes" {
        return Some(model_routes(ctx));
    }
    if method == "PUT" && path == "/api/models/backups" {
        return Some(model_backups(ctx));
    }
    if method == "GET" && path == "/api/models/azure" {
        return Some(Reply::ok(json!({ "models": azure_models() })));
    }
    if method == "POST" && (path == "/api/models/oauth" || path == "/api/models/oauth/complete") {
        return Some(Reply::error(
            501,
            "Subscription sign-in is not available in the native build. Add an API key instead.",
        ));
    }
    if method == "GET" && path.starts_with("/api/models/oauth/status/") {
        return Some(Reply::ok(json!({
            "state": "failed",
            "error": "Subscription sign-in is not available in the native build."
        })));
    }
    if method == "DELETE" && path.starts_with("/api/models/oauth/") {
        return Some(Reply::ok(models_view(ctx.store)));
    }
    if let Some(rest) = path.strip_prefix("/api/models/connections/") {
        return Some(connection_action(ctx, rest));
    }

    None
}

fn account_update(ctx: &Ctx) -> Reply {
    let mut patch = Map::new();
    for key in [
        "name",
        "email",
        "avatarUrl",
        "theme",
        "usageData",
        "traces",
        "onboardingCompleted",
    ] {
        if let Some(value) = ctx.body.get(key) {
            patch.insert(key.to_string(), value.clone());
        }
    }
    ctx.store
        .update_setting("session", &Value::Object(patch));
    ctx.refresh();
    Reply::ok(session_view(ctx.store))
}

fn model_setup(ctx: &Ctx) -> Reply {
    let state = models_view(ctx.store);
    let selected = state
        .get("selection")
        .and_then(|selection| selection.get("connectionId"))
        .and_then(Value::as_str);
    if selected.is_some() && selected != Some(super::DEMO_CONNECTION) {
        return Reply::ok(json!({ "ok": true }));
    }
    Reply::error(409, "Connect an AI provider before continuing.")
}

fn model_key(ctx: &Ctx) -> Reply {
    let provider = ctx
        .body
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let key = ctx
        .body
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if provider.is_empty() {
        return Reply::error(400, "Choose a provider.");
    }
    if key.is_empty() {
        return Reply::error(400, "Enter the API key.");
    }

    let models = match crate::llm::list_model_choices(&provider, &key, &ctx.body) {
        Ok(models) if !models.is_empty() => models,
        Ok(_) => {
            return Reply::error(
                502,
                format!("{} returned no models for this key.", provider_name(&provider)),
            )
        }
        Err(error) => return Reply::error(502, error),
    };

    let choices: Vec<Value> = models.iter().map(model_choice_json).collect();

    let connection_id = uuid();
    let mut state = super::models_state(ctx.store);
    let connection = json!({
        "id": connection_id,
        "provider": provider,
        "method": "api_key",
        "label": format!("{} API", provider_name(&provider)),
        "status": "connected",
        "models": choices,
        "managed": false,
        "keyEnding": key.chars().rev().take(4).collect::<String>().chars().rev().collect::<String>(),
        "key": key
    });
    let connections = state["connections"].as_array_mut();
    if let Some(connections) = connections {
        connections.push(connection);
    } else {
        state["connections"] = json!([connection]);
    }
    let first = models.first().map(|m| m.id.clone()).unwrap_or_default();
    state["selection"] = json!({
        "connectionId": connection_id,
        "modelId": first,
        "effort": "high"
    });
    ctx.store.set_setting("models", &state);
    ctx.refresh();
    Reply::ok(models_view(ctx.store))
}

fn model_endpoint(ctx: &Ctx) -> Reply {
    let url = ctx
        .body
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_string();
    let key = ctx
        .body
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if url.is_empty() {
        return Reply::error(400, "Enter the server address.");
    }
    let models = match crate::llm::list_compatible(&url, &key) {
        Ok(models) if !models.is_empty() => models,
        Ok(_) => return Reply::error(502, "The server returned no models."),
        Err(error) => return Reply::error(502, error),
    };
    let mut choices = Vec::new();
    for (id, name) in &models {
        choices.push(json!({
            "id": id,
            "name": name,
            "efforts": ["off", "low", "medium", "high"],
            "images": false,
            "contextWindow": 200000
        }));
    }
    let connection_id = uuid();
    let label = url
        .split("//")
        .nth(1)
        .unwrap_or(&url)
        .split('/')
        .next()
        .unwrap_or(&url)
        .to_string();
    let connection = json!({
        "id": connection_id,
        "provider": "compatible",
        "method": "api_key",
        "label": label,
        "status": "connected",
        "models": choices,
        "managed": false,
        "url": url,
        "key": key
    });
    let mut state = super::models_state(ctx.store);
    let connections = state["connections"].as_array_mut();
    if let Some(connections) = connections {
        connections.push(connection);
    } else {
        state["connections"] = json!([connection]);
    }
    state["selection"] = json!({
        "connectionId": connection_id,
        "modelId": models.first().map(|(id, _)| id.clone()).unwrap_or_default(),
        "effort": "high"
    });
    ctx.store.set_setting("models", &state);
    ctx.refresh();
    Reply::ok(models_view(ctx.store))
}

fn preferences_update(ctx: &Ctx) -> Reply {
    let mut current = preferences_state(ctx.store);
    if let (Some(target), Some(patch)) = (current.as_object_mut(), ctx.body.as_object()) {
        for (key, value) in patch {
            target.insert(key.clone(), value.clone());
        }
    }
    if current.get("timeZone").and_then(Value::as_str).is_none() {
        current["timeZone"] = Value::String(local_zone().name().to_string());
    }
    ctx.store.set_setting("preferences", &current);
    ctx.refresh();
    Reply::ok(current)
}

fn save_models(ctx: &Ctx, state: &Value) -> Reply {
    ctx.store.set_setting("models", state);
    ctx.refresh();
    Reply::ok(models_view(ctx.store))
}

fn model_selection(ctx: &Ctx) -> Reply {
    let selection = ctx
        .body
        .get("selection")
        .cloned()
        .unwrap_or_else(|| ctx.body.clone());
    if selection.get("connectionId").and_then(Value::as_str).is_none()
        || selection.get("modelId").and_then(Value::as_str).is_none()
    {
        return Reply::error(400, "Choose a model.");
    }
    let mut state = super::models_state(ctx.store);
    state["selection"] = selection;
    save_models(ctx, &state)
}

fn model_routes(ctx: &Ctx) -> Reply {
    let lane = ctx
        .body
        .get("lane")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if lane != "schedule" && lane != "background" {
        return Reply::error(400, "Choose the schedule or background lane.");
    }
    let mut state = super::models_state(ctx.store);
    if !state.get("routes").map(Value::is_object).unwrap_or(false) {
        state["routes"] = json!({});
    }
    let route = ctx.body.get("route").cloned().unwrap_or(Value::Null);
    state["routes"][lane.as_str()] = route;
    save_models(ctx, &state)
}

fn model_choice_json(choice: &crate::llm::ModelChoiceMeta) -> Value {
    let mut item = json!({
        "id": choice.id,
        "name": choice.name,
        "efforts": choice.efforts,
        "images": choice.images,
        "contextWindow": choice.context_window
    });
    if let Some(pricing) = &choice.pricing {
        item["pricing"] = pricing.clone();
    }
    item
}

fn model_backups(ctx: &Ctx) -> Reply {
    let backups = ctx
        .body
        .get("backups")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let Some(items) = backups.as_array() else {
        return Reply::error(400, "Backups must be a list.");
    };
    if items.len() > 3 {
        return Reply::error(400, "Choose at most 3 backup models.");
    }
    let mut state = super::models_state(ctx.store);
    let mut seen = std::collections::HashSet::new();
    let mut cleaned = Vec::new();
    for item in items {
        let Some(conn_id) = item.get("connectionId").and_then(Value::as_str) else {
            continue;
        };
        let Some(model_id) = item.get("modelId").and_then(Value::as_str) else {
            continue;
        };
        if seen.insert(format!("{conn_id}:{model_id}")) {
            cleaned.push(item.clone());
        }
    }
    state["backups"] = Value::Array(cleaned);
    save_models(ctx, &state)
}

fn connection_action(ctx: &Ctx, rest: &str) -> Reply {
    let mut parts = rest.split('/');
    let id = parts.next().unwrap_or_default().to_string();
    let action = parts.next();
    if id.is_empty() {
        return Reply::error(404, "Model connection not found.");
    }
    if id == super::DEMO_CONNECTION {
        return Reply::error(400, "The demo engine cannot be changed.");
    }
    let mut state = super::models_state(ctx.store);
    let connections = state
        .get("connections")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(connection) = connections
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id.as_str()))
        .cloned()
    else {
        return Reply::error(404, "Model connection not found.");
    };

    match (ctx.method, action) {
        ("DELETE", None) => {
            let kept: Vec<Value> = connections
                .into_iter()
                .filter(|item| item.get("id").and_then(Value::as_str) != Some(id.as_str()))
                .collect();
            state["connections"] = Value::Array(kept);
            if state
                .get("selection")
                .and_then(|selection| selection.get("connectionId"))
                .and_then(Value::as_str)
                == Some(id.as_str())
            {
                state["selection"] = super::demo_selection();
            }
            if let Some(backups) = state.get_mut("backups").and_then(Value::as_array_mut) {
                backups.retain(|item| {
                    item.get("connectionId").and_then(Value::as_str) != Some(id.as_str())
                });
            }
            if let Some(routes) = state.get_mut("routes").and_then(Value::as_object_mut) {
                routes.retain(|_, route| {
                    route.get("connectionId").and_then(Value::as_str) != Some(id.as_str())
                });
            }
            save_models(ctx, &state)
        }
        ("POST", Some("refresh")) => {
            let provider = crate::api::text(&connection, "provider");
            let key = crate::api::text(&connection, "key");
            if let Ok(true) = crate::llm::is_refresh_rate_limited(&provider, &key, &connection) {
                return Reply::ok(models_view(ctx.store));
            }
            match crate::llm::list_model_choices(&provider, &key, &connection) {
                Ok(models) if !models.is_empty() => {
                    let choices: Vec<Value> = models.iter().map(model_choice_json).collect();
                    let rebuilt = connections
                        .into_iter()
                        .map(|item| {
                            if item.get("id").and_then(Value::as_str) == Some(id.as_str()) {
                                let mut item = item;
                                item["models"] = Value::Array(choices.clone());
                                item["status"] = Value::String("connected".to_string());
                                item
                            } else {
                                item
                            }
                        })
                        .collect();
                    state["connections"] = Value::Array(rebuilt);
                    save_models(ctx, &state)
                }
                Ok(_) => Reply::error(502, "This provider returned no models."),
                Err(error) => Reply::error(502, error),
            }
        }
        _ => Reply::error(404, "Model connection not found."),
    }
}

fn azure_models() -> Vec<Value> {
    [
        "gpt-4o",
        "gpt-4o-mini",
        "gpt-4.1",
        "gpt-4.1-mini",
        "gpt-4.1-nano",
        "o3",
        "o4-mini",
    ]
    .into_iter()
    .map(|id| json!({ "id": id, "name": id }))
    .collect()
}

pub fn provider_name(provider: &str) -> String {
    match provider {
        "openai" => "OpenAI",
        "anthropic" => "Anthropic",
        "google" => "Google Gemini",
        "openrouter" => "OpenRouter",
        "hostinger" => "Hostinger AI Router",
        "deepseek" => "DeepSeek",
        "groq" => "Groq",
        "mistral" => "Mistral",
        "xai" => "xAI",
        "together" => "Together AI",
        "fireworks" => "Fireworks AI",
        "cerebras" => "Cerebras",
        "nvidia" => "NVIDIA",
        "moonshotai" => "Moonshot AI",
        "moonshotai-cn" => "Moonshot AI China",
        "zai" => "Z.AI",
        "zai-coding-cn" => "Z.AI Coding China",
        "minimax" => "MiniMax",
        "minimax-cn" => "MiniMax China",
        "huggingface" => "Hugging Face",
        "baseten" => "Baseten",
        "vercel-ai-gateway" => "Vercel AI Gateway",
        "amazon-bedrock" => "Amazon Bedrock",
        "azure" => "Azure OpenAI",
        "elevenlabs" => "ElevenLabs",
        "opencode" => "OpenCode Zen",
        "opencode-go" => "OpenCode Go",
        "kimi-coding" => "Kimi Coding",
        "compatible" => "OpenAI-compatible",
        other => other,
    }
    .to_string()
}


