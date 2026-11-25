use crate::api::{provider_name, text};
use crate::db::Store;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader};
use std::time::Duration;

pub const DEMO: &str = "demo";
const NOD_REFERER: &str = "https://nod.n8n.io";
const NOD_TITLE: &str = "nod";

/// Metadata for an individual model returned by provider discovery.
#[derive(Debug, Clone)]
pub struct ModelChoiceMeta {
    pub id: String,
    pub name: String,
    pub efforts: Vec<&'static str>,
    pub images: bool,
    pub context_window: u64,
    pub pricing: Option<Value>,
}

/// Live streaming update emitted while running a model request.
#[derive(Debug, Clone, Copy)]
pub enum StreamEvent<'a> {
    /// Human-readable status step (e.g., switching to a model or preparing response).
    Step(&'a str),
    /// Full accumulated reasoning/thinking buffer observed so far.
    Thinking(&'a str),
    /// Incremental assistant response text chunk (append-only).
    Delta(&'a str),
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct ToolExecResult {
    pub content: String,
    pub is_error: bool,
}

struct CandidateOutput {
    text: String,
    thinking: String,
    tokens: Option<(u64, u64)>,
}

struct CandidateError {
    message: String,
    retryable: bool,
    emitted_any: bool,
}

type CandidateResult = Result<CandidateOutput, CandidateError>;

/// Output from a completion attempt including which connection/model served it.
#[derive(Debug, Clone)]
pub struct CompletionOutcome {
    pub text: String,
    pub thinking: String,
    pub provider: String,
    pub model_id: String,
    pub switched_from: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

fn estimate_tokens(text: &str) -> u64 {
    let chars = text.chars().count() as u64;
    if chars == 0 { 0 } else { (chars / 4).max(1) }
}

fn estimate_cost_usd(provider: &str, model_id: &str, input_tokens: u64, output_tokens: u64) -> f64 {
    let lower_model = model_id.to_ascii_lowercase();
    if provider == "ollama"
        || provider == "compatible"
        || provider == "nvidia"
        || lower_model.ends_with(":free")
    {
        return 0.0;
    }
    let (in_per_m, out_per_m) = if lower_model.contains("opus") {
        (15.0, 75.0)
    } else if lower_model.contains("sonnet") || lower_model.contains("gpt-4o") && !lower_model.contains("mini") {
        (3.0, 15.0)
    } else if lower_model.contains("haiku") || lower_model.contains("mini") || lower_model.contains("flash") {
        (0.25, 1.0)
    } else {
        (1.0, 3.0)
    };
    ((input_tokens as f64) * in_per_m + (output_tokens as f64) * out_per_m) / 1_000_000.0
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(45))
        .user_agent("nod")
        .build()
}

fn stream_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_write(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(180))
        .user_agent("nod")
        .build()
}

fn is_rate_limit_status(code: u16) -> bool {
    code == 429
}

fn is_attribution_host(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("openrouter.ai")
        || lower.contains("ai-gateway.vercel.sh")
        || lower.contains("router.hostinger.com")
}

fn apply_headers(mut request: ureq::Request, url: &str, headers: &[(&str, String)]) -> ureq::Request {
    let mut has_referer = false;
    let mut has_title = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("http-referer") {
            has_referer = true;
        } else if name.eq_ignore_ascii_case("x-title") {
            has_title = true;
        }
        request = request.set(name, value);
    }
    if is_attribution_host(url) {
        if !has_referer {
            request = request.set("HTTP-Referer", NOD_REFERER);
        }
        if !has_title {
            request = request.set("X-Title", NOD_TITLE);
        }
    }
    request
}

#[derive(Debug)]
struct HttpFailure {
    status: Option<u16>,
    message: String,
    retryable: bool,
}

fn classify_http_error(url: &str, error: ureq::Error) -> HttpFailure {
    match error {
        ureq::Error::Status(code, response) => {
            let detail = response.into_string().unwrap_or_default();
            let detail: String = detail.chars().take(500).collect();
            let clean = detail.trim();
            let message = if clean.is_empty() {
                format!("The provider rejected the request to {url} ({code}).")
            } else {
                format!("The provider rejected the request to {url} ({code}): {clean}")
            };
            let retryable = code == 408 || code == 429 || code >= 500 || code == 402 || code == 403 || code == 401;
            HttpFailure {
                status: Some(code),
                message,
                retryable,
            }
        }
        ureq::Error::Transport(transport) => HttpFailure {
            status: None,
            message: format!("Could not reach {url}: {transport}"),
            retryable: true,
        },
    }
}

fn get_json_detailed(url: &str, headers: &[(&str, String)]) -> Result<Value, HttpFailure> {
    let request = apply_headers(agent().get(url), url, headers);
    let response = request
        .call()
        .map_err(|error| classify_http_error(url, error))?;
    let body = response
        .into_string()
        .map_err(|error| HttpFailure {
            status: None,
            message: format!("Could not read the response from {url}: {error}"),
            retryable: true,
        })?;
    serde_json::from_str(&body).map_err(|error| HttpFailure {
        status: None,
        message: format!("The response from {url} was not JSON: {error}"),
        retryable: false,
    })
}

fn post_json_completion(url: &str, headers: &[(&str, String)], body: &Value) -> Result<Value, String> {
    let request = apply_headers(stream_agent().post(url), url, headers);
    let response = request
        .send_string(&body.to_string())
        .map_err(|error| classify_http_error(url, error).message)?;
    let text = response
        .into_string()
        .map_err(|error| format!("Could not read the response from {url}: {error}"))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("The response from {url} was not JSON: {error}"))
}

fn bearer_detailed(url: &str, key: &str) -> Result<Value, HttpFailure> {
    get_json_detailed(
        url,
        &[
            ("Authorization", format!("Bearer {}", key.trim())),
            ("Content-Type", "application/json".to_string()),
        ],
    )
}

fn nothing_detailed(url: &str) -> Result<Value, HttpFailure> {
    get_json_detailed(url, &[("Content-Type", "application/json".to_string())])
}

fn openai_base(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "openai" => "https://api.openai.com/v1",
        "openrouter" => "https://openrouter.ai/api/v1",
        "hostinger" => "https://router.hostinger.com/v1",
        "deepseek" => "https://api.deepseek.com/v1",
        "groq" => "https://api.groq.com/openai/v1",
        "mistral" => "https://api.mistral.ai/v1",
        "xai" => "https://api.x.ai/v1",
        "together" => "https://api.together.xyz/v1",
        "fireworks" => "https://api.fireworks.ai/inference/v1",
        "cerebras" => "https://api.cerebras.ai/v1",
        "nvidia" => "https://integrate.api.nvidia.com/v1",
        "moonshotai" => "https://api.moonshot.ai/v1",
        "moonshotai-cn" => "https://api.moonshot.cn/v1",
        "zai" => "https://api.z.ai/api/paas/v4",
        "zai-coding-cn" => "https://open.bigmodel.cn/api/paas/v4",
        "minimax" => "https://api.minimax.io/v1",
        "minimax-cn" => "https://api.minimaxi.com/v1",
        "huggingface" => "https://router.huggingface.co/v1",
        "baseten" => "https://inference.baseten.co/v1",
        "vercel-ai-gateway" => "https://ai-gateway.vercel.sh/v1",
        "opencode" => "https://opencode.ai/zen/v1",
        "opencode-go" => "https://opencode.ai/zen/v1",
        "qwen-token-plan" | "qwen-token-plan-cn" | "qwen-token-plan-individual" => {
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1"
        }
        "xiaomi" | "xiaomi-token-plan-ams" | "xiaomi-token-plan-cn" | "xiaomi-token-plan-sgp" => {
            "https://api.xiaomimimo.com/v1"
        }
        _ => return None,
    })
}

fn excluded(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    [
        "embed",
        "whisper",
        "tts",
        "dall-e",
        "moderation",
        "image",
        "audio",
        "realtime",
        "transcribe",
        "sora",
        "davinci",
        "babbage",
        "text-",
        "guard",
        "rerank",
    ]
    .iter()
    .any(|needle| id.contains(needle))
}

/// Discovers available models for a provider using rich metadata where available.
pub fn list_model_choices(
    provider: &str,
    key: &str,
    settings: &Value,
) -> Result<Vec<ModelChoiceMeta>, String> {
    let default_choice = |(id, name): (String, String)| ModelChoiceMeta {
        id,
        name,
        efforts: vec!["off", "low", "medium", "high"],
        images: false,
        context_window: 200_000,
        pricing: None,
    };
    match provider {
        "anthropic" => {
            let value = get_json_detailed(
                "https://api.anthropic.com/v1/models?limit=100",
                &[
                    ("x-api-key", key.to_string()),
                    ("anthropic-version", "2023-06-01".to_string()),
                ],
            )
            .map_err(|e| e.message)?;
            Ok(rows(&value, "data", "id", "display_name")
                .into_iter()
                .map(default_choice)
                .collect())
        }
        "google" => {
            let value = get_json_detailed(
                "https://generativelanguage.googleapis.com/v1beta/models?pageSize=100",
                &[
                    ("x-goog-api-key", key.trim().to_string()),
                    ("Content-Type", "application/json".to_string()),
                ],
            )
            .map_err(|e| e.message)?;
            let models = value
                .get("models")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Ok(models
                .iter()
                .filter(|model| {
                    model
                        .get("supportedGenerationMethods")
                        .and_then(Value::as_array)
                        .map(|methods| {
                            methods.iter().any(|method| method.as_str() == Some("generateContent"))
                        })
                        .unwrap_or(false)
                })
                .filter_map(|model| {
                    let id = model
                        .get("name")
                        .and_then(Value::as_str)?
                        .trim_start_matches("models/")
                        .to_string();
                    let name = model
                        .get("displayName")
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_string();
                    let ctx = model
                        .get("inputTokenLimit")
                        .and_then(Value::as_u64)
                        .unwrap_or(1_000_000);
                    Some(ModelChoiceMeta {
                        id,
                        name,
                        efforts: vec!["off", "low", "medium", "high"],
                        images: true,
                        context_window: ctx,
                        pricing: None,
                    })
                })
                .collect())
        }
        "azure" => {
            let url = text(settings, "url");
            let deployment = text(settings, "deployment");
            let model = text(settings, "model");
            let id = if deployment.is_empty() { model } else { deployment };
            if url.is_empty() || id.is_empty() {
                return Err("Enter the Azure endpoint and deployment name.".to_string());
            }
            Ok(vec![default_choice((id.clone(), id))])
        }
        "compatible" => {
            let url = settings
                .get("url")
                .or_else(|| settings.get("baseUrl"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if url.trim().is_empty() {
                return Err("Enter the server address.".to_string());
            }
            Ok(list_compatible(url, key)?
                .into_iter()
                .map(default_choice)
                .collect())
        }
        "amazon-bedrock" => Err(
            "Amazon Bedrock needs request signing, which the native build does not include yet."
                .to_string(),
        ),
        "google-vertex" => Err(
            "Google Vertex needs a service-account token. Use a Gemini API key instead.".to_string(),
        ),
        "openrouter" | "hostinger" => {
            let base = openai_base(provider).ok_or_else(|| "Unknown router.".to_string())?;
            let url = format!("{base}/models");
            let value = bearer_detailed(&url, key).map_err(|e| e.message)?;
            let models = router_models_from(&value, provider);
            if models.is_empty() {
                return Err(format!(
                    "{} returned no language models for this key.",
                    provider_name(provider)
                ));
            }
            Ok(models)
        }
        other => {
            let Some(base) = openai_base(other) else {
                return Err(format!(
                    "The native build cannot connect to {} yet. Use a custom OpenAI-compatible endpoint.",
                    provider_name(other)
                ));
            };
            let url = format!("{base}/models");
            let value = if key.is_empty() {
                nothing_detailed(&url)
            } else {
                bearer_detailed(&url, key)
            }
            .map_err(|err| {
                if err.status == Some(401) || err.status == Some(403) {
                    format!(
                        "{} did not accept this API key: {}",
                        provider_name(other),
                        err.message
                    )
                } else {
                    err.message
                }
            })?;
            let models = models_from(&value, other);
            if models.is_empty() {
                return Err(format!(
                    "{} returned no language models for this key.",
                    provider_name(other)
                ));
            }
            Ok(models.into_iter().map(default_choice).collect())
        }
    }
}

/// Checks whether refreshing a provider fails only due to temporary rate limiting (`429`).
pub fn is_refresh_rate_limited(provider: &str, key: &str, settings: &Value) -> Result<bool, String> {
    let url = match provider {
        "anthropic" => "https://api.anthropic.com/v1/models?limit=100".to_string(),
        "google" => "https://generativelanguage.googleapis.com/v1beta/models?pageSize=100".to_string(),
        "compatible" => {
            let base = normalize_base(&text(settings, "url"));
            if base.is_empty() {
                return Ok(false);
            }
            format!("{base}/models")
        }
        other => match openai_base(other) {
            Some(base) => format!("{base}/models"),
            None => return Ok(false),
        },
    };
    let headers: Vec<(&str, String)> = match provider {
        "anthropic" => vec![
            ("x-api-key", key.to_string()),
            ("anthropic-version", "2023-06-01".to_string()),
        ],
        "google" => vec![
            ("x-goog-api-key", key.trim().to_string()),
            ("Content-Type", "application/json".to_string()),
        ],
        _ if key.trim().is_empty() => vec![("Content-Type", "application/json".to_string())],
        _ => vec![
            ("Authorization", format!("Bearer {}", key.trim())),
            ("Content-Type", "application/json".to_string()),
        ],
    };
    match get_json_detailed(&url, &headers) {
        Ok(_) => Ok(false),
        Err(err) if err.status.map(is_rate_limit_status).unwrap_or(false) => Ok(true),
        Err(err) => Err(err.message),
    }
}

fn router_models_from(value: &Value, provider: &str) -> Vec<ModelChoiceMeta> {
    let per_million = if provider == "hostinger" {
        10_000.0
    } else {
        1_000_000.0
    };
    let Some(items) = value.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.contains(":batch") || excluded(id) {
            continue;
        }
        if let Some(endpoints) = item.get("supported_endpoints").and_then(Value::as_array)
            && !endpoints.iter().any(|e| e.as_str() == Some("/v1/chat/completions"))
        {
            continue;
        }
        if let Some(out_mods) = item
            .pointer("/architecture/output_modalities")
            .and_then(Value::as_array)
            && (!out_mods.iter().any(|m| m.as_str() == Some("text"))
                || !out_mods.iter().all(|m| m.as_str() == Some("text")))
        {
            continue;
        }
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string();
        let images = item
            .pointer("/architecture/input_modalities")
            .and_then(Value::as_array)
            .map(|mods| mods.iter().any(|m| m.as_str() == Some("image")))
            .unwrap_or(false);
        let context_window = item
            .get("context_length")
            .and_then(Value::as_u64)
            .filter(|v| *v > 0)
            .unwrap_or(200_000);
        let pricing = item.get("pricing").and_then(|p| {
            let prompt = p
                .get("prompt")
                .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse::<f64>().ok()));
            let completion = p
                .get("completion")
                .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse::<f64>().ok()));
            match (prompt, completion) {
                (Some(in_p), Some(out_p)) if in_p >= 0.0 && out_p >= 0.0 => {
                    let mut map = serde_json::Map::new();
                    map.insert("input".to_string(), json!(in_p * per_million));
                    map.insert("output".to_string(), json!(out_p * per_million));
                    if let Some(cr) = p
                        .get("input_cache_read")
                        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse::<f64>().ok()))
                    {
                        map.insert("cacheRead".to_string(), json!(cr * per_million));
                    }
                    if let Some(cw) = p
                        .get("input_cache_write")
                        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse::<f64>().ok()))
                    {
                        map.insert("cacheWrite".to_string(), json!(cw * per_million));
                    }
                    Some(Value::Object(map))
                }
                _ => None,
            }
        });
        out.push(ModelChoiceMeta {
            id: id.to_string(),
            name,
            efforts: vec!["off", "low", "medium", "high"],
            images,
            context_window,
            pricing,
        });
    }
    if out.is_empty() {
        return models_from(value, provider)
            .into_iter()
            .map(|(id, name)| ModelChoiceMeta {
                id,
                name,
                efforts: vec!["off", "low", "medium", "high"],
                images: false,
                context_window: 200_000,
                pricing: None,
            })
            .collect();
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.truncate(400);
    out
}

fn rows(value: &Value, array: &str, id_key: &str, name_key: &str) -> Vec<(String, String)> {
    value
        .get(array)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = item.get(id_key).and_then(Value::as_str)?.to_string();
                    let name = item
                        .get(name_key)
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_string();
                    Some((id, name))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn models_from(value: &Value, provider: &str) -> Vec<(String, String)> {
    let mut models = rows(value, "data", "id", "name");
    if models.is_empty() {
        models = rows(value, "models", "name", "displayName")
            .into_iter()
            .map(|(id, name)| (id.trim_start_matches("models/").to_string(), name))
            .collect();
    }
    if provider == "openai" {
        models.retain(|(id, _)| {
            id.starts_with("gpt-")
                || id.starts_with("o1")
                || id.starts_with("o3")
                || id.starts_with("o4")
                || id.starts_with("chatgpt")
        });
    }
    models.retain(|(id, _)| !excluded(id));
    models.sort();
    models.dedup();
    models.truncate(400);
    models
}

fn normalize_base(url: &str) -> String {
    let mut base = url.trim().trim_end_matches('/').to_string();
    for suffix in ["/chat/completions", "/models"] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            base = stripped.trim_end_matches('/').to_string();
        }
    }
    base
}

pub fn list_compatible(url: &str, key: &str) -> Result<Vec<(String, String)>, String> {
    let base = normalize_base(url);
    if base.is_empty() {
        return Err("Enter the server address.".to_string());
    }
    let direct = format!("{base}/models");
    let mut attempt = if key.is_empty() {
        nothing_detailed(&direct)
    } else {
        bearer_detailed(&direct, key)
    };
    if attempt.is_err() && !base.ends_with("/v1") {
        let versioned = format!("{base}/v1/models");
        attempt = if key.is_empty() {
            nothing_detailed(&versioned)
        } else {
            bearer_detailed(&versioned, key)
        };
    }
    let models = models_from(&attempt.map_err(|e| e.message)?, "compatible");
    if models.is_empty() {
        return Err("The server returned no models. Check the address.".to_string());
    }
    Ok(models)
}

#[derive(Clone)]
struct ModelCandidate {
    connection_id: String,
    model_id: String,
    provider: String,
    key: String,
    url: String,
}

fn build_candidate(connections: &[Value], selection: &Value) -> Result<ModelCandidate, String> {
    let connection_id = selection
        .get("connectionId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if connection_id == crate::api::DEMO_CONNECTION {
        return Err(DEMO.to_string());
    }
    let Some(connection) = connections
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(connection_id))
    else {
        return Err("The selected model connection no longer exists. Choose another in Settings.".to_string());
    };
    if connection.get("status").and_then(Value::as_str) != Some("connected") {
        return Err("The selected model connection needs to be reconnected in Settings.".to_string());
    }
    let model_id = selection
        .get("modelId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if model_id.is_empty() {
        return Err("Choose a model in Settings.".to_string());
    }
    let provider = text(connection, "provider");
    let key = {
        let primary = text(connection, "key");
        if primary.trim().is_empty() {
            text(connection, "apiKey")
        } else {
            primary
        }
    }
    .trim()
    .to_string();
    if provider != "compatible" && provider != "ollama" && key.is_empty() {
        return Err(format!(
            "The selected {} connection is missing its API key. Reconnect it in Settings > Models.",
            provider_name(&provider)
        ));
    }
    Ok(ModelCandidate {
        connection_id: connection_id.to_string(),
        model_id,
        provider,
        key,
        url: text(connection, "url"),
    })
}

#[allow(clippy::too_many_arguments)]
fn run_candidate_with_tools<E, H>(
    candidate: &ModelCandidate,
    history: &[(String, String)],
    system_prompt: Option<&str>,
    tools: &[ToolSpec],
    max_rounds: usize,
    on_event: &mut E,
    on_tool: &mut H,
) -> CandidateResult
where
    E: FnMut(StreamEvent<'_>),
    H: FnMut(&ToolCallRequest) -> ToolExecResult,
{
    let full_history: Vec<(String, String)> = if let Some(sys) = system_prompt.filter(|s| !s.trim().is_empty()) {
        if candidate.provider == "anthropic" || candidate.provider == "google" {
            history.to_vec()
        } else {
            let mut list = vec![("system".to_string(), sys.to_string())];
            list.extend_from_slice(history);
            list
        }
    } else {
        history.to_vec()
    };

    if candidate.provider == "anthropic" {
        return anthropic_complete_with_tools(
            &candidate.model_id,
            &candidate.key,
            system_prompt,
            &full_history,
            tools,
            max_rounds,
            on_event,
            on_tool,
        );
    }
    if candidate.provider == "google" {
        return google_complete_with_tools(
            &candidate.model_id,
            &candidate.key,
            system_prompt,
            &full_history,
            tools,
            max_rounds,
            on_event,
            on_tool,
        );
    }
    let base = if candidate.provider == "compatible" {
        normalize_base(&candidate.url)
    } else if candidate.provider == "azure" {
        candidate.url.trim_end_matches('/').to_string()
    } else {
        match openai_base(&candidate.provider) {
            Some(base) => base.to_string(),
            None => {
                return Err(CandidateError {
                    message: format!(
                        "The native build cannot chat with {} yet. Use an OpenAI-compatible endpoint in Settings.",
                        provider_name(&candidate.provider)
                    ),
                    retryable: false,
                    emitted_any: false,
                });
            }
        }
    };
    if base.is_empty() {
        return Err(CandidateError {
            message: "The selected model connection has no server address.".to_string(),
            retryable: false,
            emitted_any: false,
        });
    }
    run_openai_tool_loop(
        &base,
        &candidate.model_id,
        &candidate.key,
        &full_history,
        tools,
        max_rounds,
        on_event,
        on_tool,
    )
}

fn is_context_overflow_message(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("context length")
        || lower.contains("maximum context")
        || lower.contains("too many tokens")
        || lower.contains("token limit")
}

fn is_tools_unsupported_message(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    (lower.contains("(400)") || lower.contains("(404)") || lower.contains("(422)"))
        && (lower.contains("tool") || lower.contains("function"))
}

/// Runs a completion using primary selection and configured backup models on pre-answer failure.
pub fn complete_with_selection(
    store: &Store,
    primary_override: Option<&Value>,
    backups_override: Option<&[Value]>,
    system_prompt: Option<&str>,
    history: &[(String, String)],
) -> Result<CompletionOutcome, String> {
    complete_with_selection_stream(
        store,
        primary_override,
        backups_override,
        system_prompt,
        history,
        |_| {},
    )
}

/// Runs a streaming completion with real-time step, thinking, and delta callbacks.
pub fn complete_with_selection_stream<F>(
    store: &Store,
    primary_override: Option<&Value>,
    backups_override: Option<&[Value]>,
    system_prompt: Option<&str>,
    history: &[(String, String)],
    on_event: F,
) -> Result<CompletionOutcome, String>
where
    F: FnMut(StreamEvent<'_>),
{
    complete_with_tools_stream(
        store,
        primary_override,
        backups_override,
        system_prompt,
        history,
        &[],
        1,
        on_event,
        |_| ToolExecResult {
            content: String::new(),
            is_error: true,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn complete_with_tools_stream<E, H>(
    store: &Store,
    primary_override: Option<&Value>,
    backups_override: Option<&[Value]>,
    system_prompt: Option<&str>,
    history: &[(String, String)],
    tools: &[ToolSpec],
    max_rounds: usize,
    mut on_event: E,
    mut on_tool: H,
) -> Result<CompletionOutcome, String>
where
    E: FnMut(StreamEvent<'_>),
    H: FnMut(&ToolCallRequest) -> ToolExecResult,
{
    let state = crate::api::models_internal(store);
    let connections = state
        .get("connections")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let primary_sel = primary_override
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| state.get("selection").cloned().unwrap_or(Value::Null));
    let primary = build_candidate(&connections, &primary_sel)?;

    let mut chain = vec![primary.clone()];
    let mut seen = HashSet::from([format!("{}:{}", primary.connection_id, primary.model_id)]);
    let raw_backups: Vec<Value> = backups_override
        .map(|s| s.to_vec())
        .or_else(|| state.get("backups").and_then(Value::as_array).cloned())
        .unwrap_or_default();

    for item in raw_backups.into_iter().take(3) {
        if let Ok(cand) = build_candidate(&connections, &item) {
            let k = format!("{}:{}", cand.connection_id, cand.model_id);
            if seen.insert(k) {
                chain.push(cand);
            }
        }
    }

    let mut errors: Vec<String> = Vec::new();
    let primary_name = format!("{} / {}", primary.provider, primary.model_id);
    for (index, candidate) in chain.iter().enumerate() {
        let label = format!("Sending to {} ({})", provider_name(&candidate.provider), candidate.model_id);
        on_event(StreamEvent::Step(&label));
        let mut attempt = run_candidate_with_tools(
            candidate,
            history,
            system_prompt,
            tools,
            max_rounds,
            &mut on_event,
            &mut on_tool,
        );
        if let Err(ref err) = attempt
            && !err.emitted_any
            && !tools.is_empty()
            && is_tools_unsupported_message(&err.message)
        {
            attempt = run_candidate_with_tools(
                candidate,
                history,
                system_prompt,
                &[],
                1,
                &mut on_event,
                &mut on_tool,
            );
        }
        match attempt {
            Ok(out) => {
                let sys_chars: u64 = system_prompt.map(estimate_tokens).unwrap_or(0);
                let hist_tokens: u64 = history.iter().map(|(_, m)| estimate_tokens(m)).sum();
                let (input_tokens, output_tokens) = match out.tokens {
                    Some((in_t, out_t)) if in_t > 0 || out_t > 0 => (in_t.max(1), out_t.max(1)),
                    _ => (
                        (sys_chars + hist_tokens).max(1),
                        (estimate_tokens(&out.text) + estimate_tokens(&out.thinking)).max(1),
                    ),
                };
                let cost_usd = estimate_cost_usd(&candidate.provider, &candidate.model_id, input_tokens, output_tokens);
                return Ok(CompletionOutcome {
                    text: out.text,
                    thinking: out.thinking,
                    provider: candidate.provider.clone(),
                    model_id: candidate.model_id.clone(),
                    switched_from: if index > 0 { Some(primary_name) } else { None },
                    input_tokens,
                    output_tokens,
                    cost_usd,
                });
            }
            Err(err) => {
                errors.push(format!("{}/{}: {}", candidate.provider, candidate.model_id, err.message));
                if index == 0 && is_context_overflow_message(&err.message) {
                    return Err(err.message);
                }
                if err.emitted_any || !err.retryable || index + 1 >= chain.len() {
                    return Err(if errors.len() > 1 {
                        errors.join("; then ")
                    } else {
                        err.message
                    });
                }
            }
        }
    }
    Err("No model is available.".to_string())
}

#[cfg(test)]
fn complete(store: &Store, history: &[(String, String)]) -> Result<String, String> {
    complete_with_selection(store, None, None, None, history).map(|outcome| outcome.text)
}

/// Separates `<think>...</think>` blocks from final visible reply text.
pub fn split_think_tags(raw: &str) -> (String, String) {
    let mut visible = String::with_capacity(raw.len());
    let mut thinking = String::new();
    let mut splitter = ThinkStreamSplitter::default();
    let (v, t) = splitter.push(raw);
    visible.push_str(&v);
    thinking.push_str(&t);
    let (v_tail, t_tail) = splitter.finish();
    visible.push_str(&v_tail);
    thinking.push_str(&t_tail);
    (visible.trim().to_string(), thinking.trim().to_string())
}

#[derive(Default)]
struct ThinkStreamSplitter {
    pending: String,
    in_think: bool,
}

impl ThinkStreamSplitter {
    fn push(&mut self, chunk: &str) -> (String, String) {
        self.pending.push_str(chunk);
        self.drain(false)
    }

    fn finish(&mut self) -> (String, String) {
        self.drain(true)
    }

    fn drain(&mut self, flush: bool) -> (String, String) {
        let mut out_visible = String::new();
        let mut out_thinking = String::new();

        loop {
            let tag = if self.in_think { "</think>" } else { "<think>" };
            let lower = self.pending.to_ascii_lowercase();
            if let Some(pos) = lower.find(tag) {
                let slice = &self.pending[..pos];
                if self.in_think {
                    out_thinking.push_str(slice);
                } else {
                    out_visible.push_str(slice);
                }
                let after = pos + tag.len();
                self.pending = self.pending[after..].to_string();
                self.in_think = !self.in_think;
                continue;
            }
            if !flush {
                // Keep any trailing partial prefix like `<thi` or `</th` until more arrives.
                let keep = longest_tag_suffix(&lower, tag);
                let safe_end = self.pending.len().saturating_sub(keep);
                // Ensure UTF-8 char boundary.
                let safe_end = floor_char_boundary(&self.pending, safe_end);
                if safe_end > 0 {
                    let ready = &self.pending[..safe_end];
                    if self.in_think {
                        out_thinking.push_str(ready);
                    } else {
                        out_visible.push_str(ready);
                    }
                    self.pending = self.pending[safe_end..].to_string();
                }
            } else if !self.pending.is_empty() {
                if self.in_think {
                    out_thinking.push_str(&self.pending);
                } else {
                    out_visible.push_str(&self.pending);
                }
                self.pending.clear();
            }
            break;
        }
        (out_visible, out_thinking)
    }
}

fn floor_char_boundary(s: &str, mut index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn longest_tag_suffix(haystack_lower: &str, tag: &str) -> usize {
    let max = (tag.len() - 1).min(haystack_lower.len());
    for len in (1..=max).rev() {
        if haystack_lower.ends_with(&tag[..len]) {
            return len;
        }
    }
    0
}

fn extract_delta_reasoning(delta: &Value) -> String {
    if let Some(s) = delta.get("reasoning_content").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(s) = delta.get("reasoning").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(arr) = delta.get("reasoning_details").and_then(Value::as_array) {
        return arr
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("");
    }
    String::new()
}

fn extract_message_reasoning(msg: &Value) -> String {
    if let Some(s) = msg.get("reasoning_content").and_then(Value::as_str) {
        return s.trim().to_string();
    }
    if let Some(s) = msg.get("reasoning").and_then(Value::as_str) {
        return s.trim().to_string();
    }
    String::new()
}

struct RawToolCall {
    id: String,
    name: String,
    arguments_raw: String,
}

struct OpenAiRoundOutput {
    text: String,
    thinking: String,
    tool_calls: Vec<RawToolCall>,
    emitted_delta: bool,
    in_tokens: u64,
    out_tokens: u64,
}

fn parse_openai_tool_calls_array(arr: &[Value]) -> Vec<RawToolCall> {
    arr.iter()
        .enumerate()
        .filter_map(|(i, tc)| {
            let name = tc.pointer("/function/name").and_then(Value::as_str)?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            let id = tc
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{i}"));
            let arguments_raw = tc
                .pointer("/function/arguments")
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_else(|| "{}".to_string());
            Some(RawToolCall {
                id,
                name,
                arguments_raw,
            })
        })
        .collect()
}

fn complete_openai_compatible_nonstream_round<F>(
    url: &str,
    model_id: &str,
    key: &str,
    messages: &[Value],
    tools_json: &[Value],
    on_event: &mut F,
) -> Result<OpenAiRoundOutput, (String, bool, bool)>
where
    F: FnMut(StreamEvent<'_>),
{
    let mut body = json!({
        "model": model_id,
        "messages": messages,
        "max_tokens": 8192,
        "stream": false
    });
    if !tools_json.is_empty() {
        body["tools"] = Value::Array(tools_json.to_vec());
        body["tool_choice"] = json!("auto");
    }
    let headers = auth_headers(key);
    let request = apply_headers(stream_agent().post(url), url, &headers);
    let response = request
        .send_string(&body.to_string())
        .map_err(|err| {
            let f = classify_http_error(url, err);
            (f.message, f.retryable, false)
        })?;
    let raw_text = response
        .into_string()
        .map_err(|err| (format!("Could not read the response from {url}: {err}"), true, false))?;
    parse_openai_json_round(url, &raw_text, on_event)
}

fn parse_openai_json_round<F>(
    url: &str,
    raw_text: &str,
    on_event: &mut F,
) -> Result<OpenAiRoundOutput, (String, bool, bool)>
where
    F: FnMut(StreamEvent<'_>),
{
    let value: Value = serde_json::from_str(raw_text)
        .map_err(|err| (format!("The response from {url} was not JSON: {err}"), false, false))?;
    if let Some(err_msg) = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        return Err((err_msg.to_string(), true, false));
    }
    let msg = value
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or(Value::Null);
    let raw_content = msg
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut thinking = extract_message_reasoning(&msg);
    let (clean_text, tagged_think) = split_think_tags(&raw_content);
    if !tagged_think.is_empty() {
        if !thinking.is_empty() {
            thinking.push('\n');
        }
        thinking.push_str(&tagged_think);
    }
    if !thinking.is_empty() {
        on_event(StreamEvent::Thinking(&thinking));
    }
    let tool_calls = msg
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|arr| parse_openai_tool_calls_array(arr))
        .unwrap_or_default();
    if clean_text.is_empty() && tool_calls.is_empty() {
        return Err((format!("The model returned no text: {}", brief(&value)), false, false));
    }
    let emitted_delta = !clean_text.is_empty();
    if emitted_delta {
        on_event(StreamEvent::Delta(&clean_text));
    }
    let in_tokens = value.pointer("/usage/prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let out_tokens = value.pointer("/usage/completion_tokens").and_then(Value::as_u64).unwrap_or(0);
    Ok(OpenAiRoundOutput {
        text: clean_text,
        thinking,
        tool_calls,
        emitted_delta,
        in_tokens,
        out_tokens,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_openai_tool_loop<E, H>(
    base: &str,
    model_id: &str,
    key: &str,
    history: &[(String, String)],
    tools: &[ToolSpec],
    max_rounds: usize,
    on_event: &mut E,
    on_tool: &mut H,
) -> CandidateResult
where
    E: FnMut(StreamEvent<'_>),
    H: FnMut(&ToolCallRequest) -> ToolExecResult,
{
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let mut messages: Vec<Value> = history
        .iter()
        .map(|(role, text)| json!({ "role": role, "content": text }))
        .collect();
    let tools_json: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters
                }
            })
        })
        .collect();

    let rounds = max_rounds.max(1);
    let mut emitted_any = false;
    let mut all_thinking = String::new();
    let mut last_answer = String::new();
    let mut total_in = 0_u64;
    let mut total_out = 0_u64;
    let mut ran_tool = false;

    for round in 0..rounds {
        let round_tools: &[Value] = if round + 1 < rounds || rounds == 1 {
            &tools_json
        } else {
            &[]
        };
        let out = complete_openai_round(&url, model_id, key, &messages, round_tools, on_event)
            .map_err(|(message, retryable, round_emitted)| CandidateError {
                message,
                retryable,
                emitted_any: emitted_any || round_emitted,
            })?;
        if out.emitted_delta {
            emitted_any = true;
        }
        total_in += out.in_tokens;
        total_out += out.out_tokens;
        if !out.thinking.is_empty() {
            if !all_thinking.is_empty() {
                all_thinking.push('\n');
            }
            all_thinking.push_str(&out.thinking);
        }
        if !out.text.is_empty() {
            last_answer = out.text.clone();
        }
        if out.tool_calls.is_empty() {
            let final_text = if out.text.is_empty() && ran_tool {
                last_answer
            } else {
                out.text
            };
            return Ok(CandidateOutput {
                text: final_text,
                thinking: all_thinking,
                tokens: if total_in > 0 || total_out > 0 {
                    Some((total_in, total_out))
                } else {
                    None
                },
            });
        }

        let tc_json: Vec<Value> = out
            .tool_calls
            .iter()
            .map(|tc| {
                json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": tc.arguments_raw
                    }
                })
            })
            .collect();
        messages.push(json!({
            "role": "assistant",
            "content": if out.text.is_empty() { Value::Null } else { json!(out.text) },
            "tool_calls": tc_json
        }));

        for tc in out.tool_calls {
            emitted_any = true;
            ran_tool = true;
            let args: Value = if tc.arguments_raw.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&tc.arguments_raw)
                    .unwrap_or_else(|_| json!({ "_raw": tc.arguments_raw }))
            };
            on_event(StreamEvent::Step(&format!("Running {}…", tc.name)));
            let res = on_tool(&ToolCallRequest {
                id: tc.id.clone(),
                name: tc.name,
                arguments: args,
            });
            messages.push(json!({
                "role": "tool",
                "tool_call_id": tc.id,
                "content": res.content
            }));
        }
    }

    Ok(CandidateOutput {
        text: last_answer,
        thinking: all_thinking,
        tokens: if total_in > 0 || total_out > 0 {
            Some((total_in, total_out))
        } else {
            None
        },
    })
}

#[cfg(test)]
fn complete_openai_compatible_stream<F>(
    base: &str,
    model_id: &str,
    key: &str,
    history: &[(String, String)],
    on_event: &mut F,
) -> Result<(String, String), (String, bool)>
where
    F: FnMut(StreamEvent<'_>),
{
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let messages: Vec<Value> = history
        .iter()
        .map(|(role, text)| json!({ "role": role, "content": text }))
        .collect();
    complete_openai_round(&url, model_id, key, &messages, &[], on_event)
        .map(|out| (out.text, out.thinking))
        .map_err(|(msg, retryable, _)| (msg, retryable))
}

fn complete_openai_round<F>(
    url: &str,
    model_id: &str,
    key: &str,
    messages: &[Value],
    tools_json: &[Value],
    on_event: &mut F,
) -> Result<OpenAiRoundOutput, (String, bool, bool)>
where
    F: FnMut(StreamEvent<'_>),
{
    let mut body = json!({
        "model": model_id,
        "messages": messages,
        "max_tokens": 8192,
        "stream": true
    });
    if !tools_json.is_empty() {
        body["tools"] = Value::Array(tools_json.to_vec());
        body["tool_choice"] = json!("auto");
    }
    let mut headers = auth_headers(key);
    headers.push(("Accept", "text/event-stream".to_string()));
    headers.push(("Cache-Control", "no-cache".to_string()));
    let request = apply_headers(stream_agent().post(url), url, &headers);
    let response = match request.send_string(&body.to_string()) {
        Ok(resp) => resp,
        Err(err) => {
            let f = classify_http_error(url, err);
            return Err((f.message, f.retryable, false));
        }
    };

    let is_json = response
        .content_type()
        .to_ascii_lowercase()
        .contains("application/json");
    if is_json {
        let raw_text = response.into_string().map_err(|err| {
            (format!("Could not read the response from {url}: {err}"), true, false)
        })?;
        return parse_openai_json_round(url, &raw_text, on_event);
    }

    let mut reader = BufReader::new(response.into_reader());
    let mut line = String::new();
    let mut splitter = ThinkStreamSplitter::default();
    let mut answer = String::new();
    let mut thinking = String::new();
    let mut tool_calls_map: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
    let mut leading_trim_done = false;
    let mut emitted_thinking_step = false;
    let mut emitted_writing_step = false;
    let mut saw_done = false;
    let mut in_tokens = 0_u64;
    let mut out_tokens = 0_u64;

    loop {
        line.clear();
        let read = match reader.read_line(&mut line) {
            Ok(n) => n,
            Err(err) => {
                let (vis_tail, think_tail) = splitter.finish();
                if !think_tail.is_empty() {
                    thinking.push_str(&think_tail);
                    on_event(StreamEvent::Thinking(&thinking));
                }
                if !vis_tail.is_empty() {
                    let emit_tail = if !leading_trim_done {
                        vis_tail.trim_start().to_string()
                    } else {
                        vis_tail
                    };
                    if !emit_tail.is_empty() {
                        answer.push_str(&emit_tail);
                        on_event(StreamEvent::Delta(&emit_tail));
                    }
                }
                if answer.trim().is_empty() && tool_calls_map.is_empty() {
                    on_event(StreamEvent::Step("Retrying response without streaming…"));
                    return complete_openai_compatible_nonstream_round(
                        url, model_id, key, messages, tools_json, on_event,
                    )
                    .map_err(|(fallback_err, retryable, emitted)| {
                        (
                            format!("Stream interrupted from {url}: {err} ({fallback_err})"),
                            retryable,
                            emitted,
                        )
                    });
                }
                let partial = format!(
                    "{}\n\n[Stream interrupted from {url}: {err}]",
                    answer.trim_end()
                );
                return Ok(OpenAiRoundOutput {
                    emitted_delta: !answer.is_empty(),
                    text: partial,
                    thinking: thinking.trim().to_string(),
                    tool_calls: Vec::new(),
                    in_tokens,
                    out_tokens,
                });
            }
        };
        if read == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(payload) = trimmed.strip_prefix("data:") else {
            if trimmed.starts_with('{')
                && let Ok(val) = serde_json::from_str::<Value>(trimmed)
                && let Some(err_msg) = val.pointer("/error/message").and_then(Value::as_str)
            {
                return Err((err_msg.to_string(), answer.is_empty(), !answer.is_empty()));
            }
            continue;
        };
        let data = payload.trim();
        if data == "[DONE]" {
            saw_done = true;
            break;
        }
        let Ok(val) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Some(u) = val.get("usage") {
            if let Some(pt) = u.get("prompt_tokens").and_then(Value::as_u64) {
                in_tokens = in_tokens.max(pt);
            }
            if let Some(ct) = u.get("completion_tokens").and_then(Value::as_u64) {
                out_tokens = out_tokens.max(ct);
            }
        }
        if let Some(err_msg) = val.pointer("/error/message").and_then(Value::as_str) {
            return Err((err_msg.to_string(), answer.is_empty(), !answer.is_empty()));
        }
        let Some(delta) = val.pointer("/choices/0/delta") else {
            if let Some(msg) = val.pointer("/choices/0/message") {
                if let Some(content) = msg.get("content").and_then(Value::as_str)
                    && answer.is_empty()
                    && !content.trim().is_empty()
                {
                    let (v, t) = splitter.push(content);
                    if !t.is_empty() {
                        thinking.push_str(&t);
                        on_event(StreamEvent::Thinking(&thinking));
                    }
                    if !v.is_empty() {
                        answer.push_str(&v);
                        on_event(StreamEvent::Delta(&v));
                    }
                }
                if tool_calls_map.is_empty()
                    && let Some(arr) = msg.get("tool_calls").and_then(Value::as_array)
                {
                    for (i, tc) in parse_openai_tool_calls_array(arr).into_iter().enumerate() {
                        tool_calls_map.insert(i, (tc.id, tc.name, tc.arguments_raw));
                    }
                }
            }
            continue;
        };

        let r_chunk = extract_delta_reasoning(delta);
        if !r_chunk.is_empty() {
            if !emitted_thinking_step {
                emitted_thinking_step = true;
                on_event(StreamEvent::Step("Thinking…"));
            }
            thinking.push_str(&r_chunk);
            on_event(StreamEvent::Thinking(&thinking));
        }

        if let Some(tc_arr) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tc_arr {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let entry = tool_calls_map
                    .entry(idx)
                    .or_insert_with(|| (String::new(), String::new(), String::new()));
                if let Some(id) = tc.get("id").and_then(Value::as_str)
                    && !id.is_empty()
                {
                    entry.0 = id.to_string();
                }
                if let Some(name) = tc.pointer("/function/name").and_then(Value::as_str)
                    && !name.is_empty()
                {
                    entry.1.push_str(name);
                }
                if let Some(args) = tc.pointer("/function/arguments").and_then(Value::as_str) {
                    entry.2.push_str(args);
                }
            }
        }

        if let Some(c_chunk) = delta.get("content").and_then(Value::as_str)
            && !c_chunk.is_empty()
        {
            let (mut vis_chunk, think_chunk) = splitter.push(c_chunk);
            if !think_chunk.is_empty() {
                if !emitted_thinking_step {
                    emitted_thinking_step = true;
                    on_event(StreamEvent::Step("Thinking…"));
                }
                thinking.push_str(&think_chunk);
                on_event(StreamEvent::Thinking(&thinking));
            }
            if !vis_chunk.is_empty() {
                if !leading_trim_done {
                    let trimmed_start = vis_chunk.trim_start();
                    if trimmed_start.is_empty() {
                        continue;
                    }
                    vis_chunk = trimmed_start.to_string();
                    leading_trim_done = true;
                }
                if !emitted_writing_step {
                    emitted_writing_step = true;
                    on_event(StreamEvent::Step("Writing response…"));
                }
                answer.push_str(&vis_chunk);
                on_event(StreamEvent::Delta(&vis_chunk));
            }
        }
    }

    let (vis_tail, think_tail) = splitter.finish();
    if !think_tail.is_empty() {
        thinking.push_str(&think_tail);
        on_event(StreamEvent::Thinking(&thinking));
    }
    if !vis_tail.is_empty() {
        let emit_tail = if !leading_trim_done {
            vis_tail.trim_start().to_string()
        } else {
            vis_tail
        };
        if !emit_tail.is_empty() {
            answer.push_str(&emit_tail);
            on_event(StreamEvent::Delta(&emit_tail));
        }
    }

    let tool_calls: Vec<RawToolCall> = tool_calls_map
        .into_iter()
        .filter_map(|(idx, (id, name, arguments_raw))| {
            let trimmed_name = name.trim().to_string();
            if trimmed_name.is_empty() {
                return None;
            }
            Some(RawToolCall {
                id: if id.is_empty() {
                    format!("call_{idx}")
                } else {
                    id
                },
                name: trimmed_name,
                arguments_raw,
            })
        })
        .collect();

    let final_answer = answer.trim().to_string();
    let final_thinking = thinking.trim().to_string();
    if final_answer.is_empty() && tool_calls.is_empty() {
        if !saw_done {
            on_event(StreamEvent::Step("Retrying response without streaming…"));
            if let Ok(fallback) = complete_openai_compatible_nonstream_round(
                url, model_id, key, messages, tools_json, on_event,
            ) {
                return Ok(fallback);
            }
        }
        return Err(("The model returned an empty response.".to_string(), true, false));
    }
    Ok(OpenAiRoundOutput {
        emitted_delta: !final_answer.is_empty(),
        text: final_answer,
        thinking: final_thinking,
        tool_calls,
        in_tokens,
        out_tokens,
    })
}

fn auth_headers(key: &str) -> Vec<(&'static str, String)> {
    let mut headers = vec![("Content-Type", "application/json".to_string())];
    if !key.trim().is_empty() {
        headers.push(("Authorization", format!("Bearer {}", key.trim())));
    }
    headers
}

fn brief(value: &Value) -> String {
    let text = value.to_string();
    text.chars().take(300).collect()
}

#[allow(clippy::too_many_arguments)]
fn anthropic_complete_with_tools<E, H>(
    model: &str,
    key: &str,
    system_prompt: Option<&str>,
    history: &[(String, String)],
    tools: &[ToolSpec],
    max_rounds: usize,
    on_event: &mut E,
    on_tool: &mut H,
) -> CandidateResult
where
    E: FnMut(StreamEvent<'_>),
    H: FnMut(&ToolCallRequest) -> ToolExecResult,
{
    let mut messages: Vec<Value> = history
        .iter()
        .map(|(role, text)| {
            let role = if role == "assistant" { "assistant" } else { "user" };
            json!({ "role": role, "content": text })
        })
        .collect();
    let tools_json: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters
            })
        })
        .collect();
    let rounds = max_rounds.max(1);
    let mut emitted_any = false;
    let mut total_in = 0_u64;
    let mut total_out = 0_u64;
    let mut all_thinking = String::new();
    let mut last_answer = String::new();

    for round in 0..rounds {
        let mut payload = json!({ "model": model, "max_tokens": 8192, "messages": messages });
        if let Some(sys) = system_prompt.filter(|s| !s.trim().is_empty()) {
            payload["system"] = json!(sys);
        }
        if !tools_json.is_empty() && (round + 1 < rounds || rounds == 1) {
            payload["tools"] = Value::Array(tools_json.clone());
        }
        let value = post_json_completion(
            "https://api.anthropic.com/v1/messages",
            &[
                ("x-api-key", key.to_string()),
                ("anthropic-version", "2023-06-01".to_string()),
                ("Content-Type", "application/json".to_string()),
            ],
            &payload,
        )
        .map_err(|message| CandidateError {
            message,
            retryable: true,
            emitted_any,
        })?;

        total_in += value.pointer("/usage/input_tokens").and_then(Value::as_u64).unwrap_or(0);
        total_out += value.pointer("/usage/output_tokens").and_then(Value::as_u64).unwrap_or(0);

        let parts = value
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut raw_text = String::new();
        let mut round_thinking = String::new();
        let mut tool_uses = Vec::new();
        for part in &parts {
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        raw_text.push_str(t);
                    }
                }
                Some("thinking") => {
                    if let Some(t) = part.get("thinking").and_then(Value::as_str) {
                        if !round_thinking.is_empty() {
                            round_thinking.push('\n');
                        }
                        round_thinking.push_str(t);
                    }
                }
                Some("tool_use") => {
                    if let (Some(id), Some(name)) = (
                        part.get("id").and_then(Value::as_str),
                        part.get("name").and_then(Value::as_str),
                    ) {
                        tool_uses.push(ToolCallRequest {
                            id: id.to_string(),
                            name: name.to_string(),
                            arguments: part.get("input").cloned().unwrap_or_else(|| json!({})),
                        });
                    }
                }
                _ => {}
            }
        }
        let (clean_text, tag_think) = split_think_tags(&raw_text);
        if !tag_think.is_empty() {
            if !round_thinking.is_empty() {
                round_thinking.push('\n');
            }
            round_thinking.push_str(&tag_think);
        }
        if !round_thinking.is_empty() {
            if !all_thinking.is_empty() {
                all_thinking.push('\n');
            }
            all_thinking.push_str(&round_thinking);
            on_event(StreamEvent::Thinking(&all_thinking));
        }
        if !clean_text.is_empty() {
            emitted_any = true;
            last_answer = clean_text.clone();
            on_event(StreamEvent::Delta(&clean_text));
        }

        if tool_uses.is_empty() {
            let final_text = if clean_text.is_empty() { last_answer } else { clean_text };
            if final_text.trim().is_empty() {
                return Err(CandidateError {
                    message: format!("The model returned no text: {}", brief(&value)),
                    retryable: false,
                    emitted_any,
                });
            }
            return Ok(CandidateOutput {
                text: final_text.trim().to_string(),
                thinking: all_thinking.trim().to_string(),
                tokens: if total_in > 0 || total_out > 0 {
                    Some((total_in, total_out))
                } else {
                    None
                },
            });
        }

        messages.push(json!({ "role": "assistant", "content": parts }));
        let mut result_blocks = Vec::new();
        for tc in tool_uses {
            emitted_any = true;
            on_event(StreamEvent::Step(&format!("Running {}…", tc.name)));
            let res = on_tool(&tc);
            result_blocks.push(json!({
                "type": "tool_result",
                "tool_use_id": tc.id,
                "content": res.content,
                "is_error": res.is_error
            }));
        }
        messages.push(json!({ "role": "user", "content": result_blocks }));
    }

    Ok(CandidateOutput {
        text: last_answer.trim().to_string(),
        thinking: all_thinking.trim().to_string(),
        tokens: if total_in > 0 || total_out > 0 {
            Some((total_in, total_out))
        } else {
            None
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn google_complete_with_tools<E, H>(
    model: &str,
    key: &str,
    system_prompt: Option<&str>,
    history: &[(String, String)],
    tools: &[ToolSpec],
    max_rounds: usize,
    on_event: &mut E,
    on_tool: &mut H,
) -> CandidateResult
where
    E: FnMut(StreamEvent<'_>),
    H: FnMut(&ToolCallRequest) -> ToolExecResult,
{
    let mut contents: Vec<Value> = Vec::new();
    for (role, text) in history {
        let role = if role == "assistant" { "model" } else { "user" };
        if let Some(last) = contents.last_mut()
            && last.get("role").and_then(Value::as_str) == Some(role)
        {
            let existing = last
                .pointer("/parts/0/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            last["parts"] = json!([{ "text": format!("{existing}\n\n{text}") }]);
            continue;
        }
        contents.push(json!({ "role": role, "parts": [{ "text": text }] }));
    }

    let decls: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters
            })
        })
        .collect();
    let rounds = max_rounds.max(1);
    let mut emitted_any = false;
    let mut total_in = 0_u64;
    let mut total_out = 0_u64;
    let mut all_thinking = String::new();
    let mut last_answer = String::new();
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
    );

    for round in 0..rounds {
        let mut payload = json!({ "contents": contents });
        if let Some(sys) = system_prompt.filter(|s| !s.trim().is_empty()) {
            payload["systemInstruction"] = json!({ "parts": [{ "text": sys }] });
        }
        if !decls.is_empty() && (round + 1 < rounds || rounds == 1) {
            payload["tools"] = json!([{ "functionDeclarations": decls }]);
        }
        let value = post_json_completion(
            &url,
            &[
                ("x-goog-api-key", key.trim().to_string()),
                ("Content-Type", "application/json".to_string()),
            ],
            &payload,
        )
        .map_err(|message| CandidateError {
            message,
            retryable: true,
            emitted_any,
        })?;

        total_in += value
            .pointer("/usageMetadata/promptTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        total_out += value
            .pointer("/usageMetadata/candidatesTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        let parts = value
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut raw_text = String::new();
        let mut fn_calls = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                raw_text.push_str(t);
            }
            if let Some(fc) = part.get("functionCall")
                && let Some(name) = fc.get("name").and_then(Value::as_str)
            {
                fn_calls.push(ToolCallRequest {
                    id: format!("fn_{round}_{i}"),
                    name: name.to_string(),
                    arguments: fc.get("args").cloned().unwrap_or_else(|| json!({})),
                });
            }
        }
        let (clean_text, tag_think) = split_think_tags(&raw_text);
        if !tag_think.is_empty() {
            if !all_thinking.is_empty() {
                all_thinking.push('\n');
            }
            all_thinking.push_str(&tag_think);
            on_event(StreamEvent::Thinking(&all_thinking));
        }
        if !clean_text.is_empty() {
            emitted_any = true;
            last_answer = clean_text.clone();
            on_event(StreamEvent::Delta(&clean_text));
        }

        if fn_calls.is_empty() {
            let final_text = if clean_text.is_empty() { last_answer } else { clean_text };
            if final_text.trim().is_empty() {
                return Err(CandidateError {
                    message: format!("The model returned no text: {}", brief(&value)),
                    retryable: false,
                    emitted_any,
                });
            }
            return Ok(CandidateOutput {
                text: final_text.trim().to_string(),
                thinking: all_thinking.trim().to_string(),
                tokens: if total_in > 0 || total_out > 0 {
                    Some((total_in, total_out))
                } else {
                    None
                },
            });
        }

        contents.push(json!({ "role": "model", "parts": parts }));
        let mut resp_parts = Vec::new();
        for fc in fn_calls {
            emitted_any = true;
            on_event(StreamEvent::Step(&format!("Running {}…", fc.name)));
            let res = on_tool(&fc);
            resp_parts.push(json!({
                "functionResponse": {
                    "name": fc.name,
                    "response": {
                        "result": res.content,
                        "isError": res.is_error
                    }
                }
            }));
        }
        contents.push(json!({ "role": "user", "parts": resp_parts }));
    }

    Ok(CandidateOutput {
        text: last_answer.trim().to_string(),
        thinking: all_thinking.trim().to_string(),
        tokens: if total_in > 0 || total_out > 0 {
            Some((total_in, total_out))
        } else {
            None
        },
    })
}

pub fn demo_reply(prompt: &str) -> String {
    let short: String = prompt.trim().chars().take(140).collect();
    format!(
        "Saved: \"{short}\"\n\nNo language model is connected, so this answer comes from the local Nod demo engine. It can keep the conversation, schedules, tasks and memory on this computer, but it cannot research or act for you yet.\n\nTo connect one, open Settings, then Models, and add a key for OpenAI, Anthropic, Google, DeepSeek, OpenRouter or a local OpenAI-compatible server."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_http::{Header, Response, Server, StatusCode};

    #[test]
    fn models_internal_keeps_key_for_llm_while_models_view_redacts_it() {
        let store = Store::open_in_memory().unwrap();
        let conn_id = "c1111111-1111-4111-8111-111111111111";
        store.set_setting(
            "models",
            &json!({
                "connections": [{
                    "id": conn_id,
                    "provider": "nvidia",
                    "method": "api_key",
                    "label": "NVIDIA API",
                    "status": "connected",
                    "models": [{ "id": "meta/llama-3.1-8b-instruct", "name": "Llama 3.1 8B" }],
                    "key": "nvapi-secret-test-key"
                }],
                "selection": {
                    "connectionId": conn_id,
                    "modelId": "meta/llama-3.1-8b-instruct",
                    "effort": "high"
                },
                "routes": {}
            }),
        );

        let view = crate::api::models_view(&store);
        let view_conn = view["connections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == conn_id)
            .unwrap();
        assert!(view_conn.get("key").is_none());

        let internal = crate::api::models_internal(&store);
        let int_conn = internal["connections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == conn_id)
            .unwrap();
        assert_eq!(int_conn["key"], json!("nvapi-secret-test-key"));
    }

    #[test]
    fn complete_sends_bearer_key_and_rejects_missing_cloud_key() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut req = server.recv().unwrap();
            let auth = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .map(|h| h.value.as_str().to_string());
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            if auth.as_deref() == Some("Bearer nvapi-test-123") {
                let mut resp = Response::from_string(
                    json!({
                        "choices": [{ "message": { "content": "Hello from NVIDIA mock" } }]
                    })
                    .to_string(),
                )
                .with_status_code(StatusCode(200));
                if let Ok(h) = Header::from_bytes("Content-Type", "application/json") {
                    resp.add_header(h);
                }
                let _ = req.respond(resp);
            } else {
                let _ = req.respond(Response::from_string("Unauthorized").with_status_code(StatusCode(401)));
            }
        });

        let store = Store::open_in_memory().unwrap();
        let conn_id = "c3333333-3333-4333-8333-333333333333";
        let base = format!("http://127.0.0.1:{port}/v1");
        store.set_setting(
            "models",
            &json!({
                "connections": [{
                    "id": conn_id,
                    "provider": "compatible",
                    "method": "api_key",
                    "label": "Mock NVIDIA",
                    "status": "connected",
                    "url": base,
                    "models": [{ "id": "meta/llama-3.1-8b-instruct", "name": "Llama 3.1 8B" }],
                    "key": "nvapi-test-123"
                }],
                "selection": {
                    "connectionId": conn_id,
                    "modelId": "meta/llama-3.1-8b-instruct",
                    "effort": "high"
                },
                "routes": {}
            }),
        );
        let reply = complete(&store, &[("user".to_string(), "hi".to_string())]).unwrap();
        assert_eq!(reply, "Hello from NVIDIA mock");
        handle.join().unwrap();

        let err_store = Store::open_in_memory().unwrap();
        let conn_id = "c2222222-2222-4222-8222-222222222222";
        err_store.set_setting(
            "models",
            &json!({
                "connections": [{
                    "id": conn_id,
                    "provider": "nvidia",
                    "method": "api_key",
                    "label": "NVIDIA API",
                    "status": "connected",
                    "models": [{ "id": "meta/llama-3.1-8b-instruct", "name": "Llama 3.1 8B" }],
                    "key": ""
                }],
                "selection": {
                    "connectionId": conn_id,
                    "modelId": "meta/llama-3.1-8b-instruct",
                    "effort": "high"
                },
                "routes": {}
            }),
        );
        let err = complete(&err_store, &[("user".to_string(), "hi".to_string())]).unwrap_err();
        assert!(err.contains("missing its API key"));
    }

    #[test]
    fn backup_model_fallback_runs_on_primary_failure_and_sends_attribution() {
        let primary_srv = Server::http("127.0.0.1:0").unwrap();
        let p_port = primary_srv.server_addr().to_ip().unwrap().port();
        let backup_srv = Server::http("127.0.0.1:0").unwrap();
        let b_port = backup_srv.server_addr().to_ip().unwrap().port();

        let p_handle = std::thread::spawn(move || {
            let req = primary_srv.recv().unwrap();
            let _ = req.respond(
                Response::from_string("{\"error\":\"quota exceeded\"}").with_status_code(StatusCode(429)),
            );
        });
        let b_handle = std::thread::spawn(move || {
            let mut req = backup_srv.recv().unwrap();
            let ua = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("User-Agent"))
                .map(|h| h.value.as_str().to_string());
            assert_eq!(ua.as_deref(), Some("nod"));
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let mut resp = Response::from_string(
                json!({ "choices": [{ "message": { "content": "Backup answered!" } }] }).to_string(),
            )
            .with_status_code(StatusCode(200));
            if let Ok(h) = Header::from_bytes("Content-Type", "application/json") {
                resp.add_header(h);
            }
            let _ = req.respond(resp);
        });

        let store = Store::open_in_memory().unwrap();
        store.set_setting(
            "models",
            &json!({
                "connections": [
                    {
                        "id": "conn-p",
                        "provider": "compatible",
                        "method": "api_key",
                        "label": "Primary",
                        "status": "connected",
                        "url": format!("http://127.0.0.1:{p_port}/v1"),
                        "models": [{ "id": "p-model", "name": "Primary" }],
                        "key": "sk-p"
                    },
                    {
                        "id": "conn-b",
                        "provider": "compatible",
                        "method": "api_key",
                        "label": "Backup",
                        "status": "connected",
                        "url": format!("http://127.0.0.1:{b_port}/v1"),
                        "models": [{ "id": "b-model", "name": "Backup" }],
                        "key": "sk-b"
                    }
                ],
                "selection": { "connectionId": "conn-p", "modelId": "p-model", "effort": "high" },
                "backups": [{ "connectionId": "conn-b", "modelId": "b-model", "effort": "high" }],
                "routes": {}
            }),
        );
        let outcome = complete_with_selection(
            &store,
            None,
            None,
            Some("You are helpful"),
            &[("user".to_string(), "hello".to_string())],
        )
        .unwrap();
        assert_eq!(outcome.text, "Backup answered!");
        assert_eq!(outcome.model_id, "b-model");
        assert!(outcome.switched_from.is_some());
        p_handle.join().unwrap();
        b_handle.join().unwrap();
    }

    #[test]
    fn split_think_tags_and_sse_streaming_work() {
        let (clean, think) = split_think_tags("<think>step 1\nstep 2</think>\n\nHello there!");
        assert_eq!(clean, "Hello there!");
        assert_eq!(think, "step 1\nstep 2");

        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut req = server.recv().unwrap();
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            assert!(body.contains("\"stream\":true"));
            let sse = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"<thi\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"nk>plan\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"ning</think>Hi \"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"world!\"}}]}\n\n",
                "data: [DONE]\n\n"
            );
            let mut resp = Response::from_string(sse).with_status_code(StatusCode(200));
            if let Ok(h) = Header::from_bytes("Content-Type", "text/event-stream") {
                resp.add_header(h);
            }
            let _ = req.respond(resp);
        });

        let mut streamed_text = String::new();
        let mut seen_think = String::new();
        let (answer, thinking) = complete_openai_compatible_stream(
            &format!("http://127.0.0.1:{port}/v1"),
            "demo-model",
            "sk-test",
            &[("user".to_string(), "hi".to_string())],
            &mut |evt| match evt {
                StreamEvent::Delta(d) => streamed_text.push_str(d),
                StreamEvent::Thinking(t) => seen_think = t.to_string(),
                StreamEvent::Step(_) => {}
            },
        )
        .unwrap();
        assert_eq!(answer, "Hi world!");
        assert_eq!(streamed_text, "Hi world!");
        assert_eq!(thinking, "planning");
        assert_eq!(seen_think, "planning");
        handle.join().unwrap();
    }

    #[test]
    fn stream_interrupted_before_output_falls_back_to_nonstream() {
        use std::io::{Read, Write};
        use std::net::{Shutdown, TcpListener, TcpStream};

        fn read_full_req(stream: &mut TcpStream) -> String {
            let mut data = Vec::new();
            let mut chunk = [0u8; 1024];
            let mut expected_total: Option<usize> = None;
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&data);
                if expected_total.is_none()
                    && let Some(hdr_end) = text.find("\r\n\r\n")
                {
                    let cl = text[..hdr_end]
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    expected_total = Some(hdr_end + 4 + cl);
                }
                if let Some(total) = expected_total
                    && data.len() >= total
                {
                    break;
                }
            }
            String::from_utf8_lossy(&data).to_string()
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            // First request (stream: true): send headers with broken chunked encoding so read_line fails.
            let (mut s1, _) = listener.accept().unwrap();
            let _ = read_full_req(&mut s1);
            let _ = s1.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nFF\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\r\n",
            );
            let _ = s1.shutdown(Shutdown::Both);
            drop(s1);

            // Second request (stream: false fallback): respond cleanly with complete JSON.
            let (mut s2, _) = listener.accept().unwrap();
            let req2 = read_full_req(&mut s2);
            assert!(req2.contains("\"stream\":false"), "unexpected req2: {req2}");
            let body = json!({
                "choices": [{ "message": { "content": "Recovered reading list!" } }]
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s2.write_all(resp.as_bytes());
            let _ = s2.flush();
            let _ = s2.shutdown(Shutdown::Write);
        });

        let (answer, _) = complete_openai_compatible_stream(
            &format!("http://127.0.0.1:{port}/v1"),
            "demo-model",
            "sk-test",
            &[("user".to_string(), "make a page".to_string())],
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(answer, "Recovered reading list!");
        srv.join().unwrap();
    }

    #[test]
    fn openai_streaming_multi_turn_tool_calls_work() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut req1 = server.recv().unwrap();
            let mut body1 = String::new();
            let _ = req1.as_reader().read_to_string(&mut body1);
            assert!(body1.contains("\"name\":\"create_page\""));
            let sse1 = concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"create_page\",\"arguments\":\"{\\\"title\\\":\\\"Re\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ading\\\",\\\"html\\\":\\\"<h1>Hi</h1>\\\"}\"}}]}}]}\n\n",
                "data: [DONE]\n\n"
            );
            let mut r1 = Response::from_string(sse1).with_status_code(StatusCode(200));
            if let Ok(h) = Header::from_bytes("Content-Type", "text/event-stream") {
                r1.add_header(h);
            }
            let _ = req1.respond(r1);

            let mut req2 = server.recv().unwrap();
            let mut body2 = String::new();
            let _ = req2.as_reader().read_to_string(&mut body2);
            assert!(body2.contains("\"tool_call_id\":\"call_abc\""));
            assert!(body2.contains("page-saved-id"));
            let sse2 = "data: {\"choices\":[{\"delta\":{\"content\":\"Created your page!\"}}]}\n\ndata: [DONE]\n\n";
            let mut r2 = Response::from_string(sse2).with_status_code(StatusCode(200));
            if let Ok(h) = Header::from_bytes("Content-Type", "text/event-stream") {
                r2.add_header(h);
            }
            let _ = req2.respond(r2);
        });

        let store = Store::open_in_memory().unwrap();
        store.set_setting(
            "models",
            &json!({
                "connections": [{
                    "id": "conn-tools",
                    "provider": "compatible",
                    "method": "api_key",
                    "label": "Mock Tools",
                    "status": "connected",
                    "url": format!("http://127.0.0.1:{port}/v1"),
                    "models": [{ "id": "tool-model", "name": "Tool Model" }],
                    "key": "sk-tools"
                }],
                "selection": { "connectionId": "conn-tools", "modelId": "tool-model", "effort": "high" },
                "routes": {}
            }),
        );

        let tools = vec![ToolSpec {
            name: "create_page".to_string(),
            description: "Save an HTML page".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "html": { "type": "string" }
                },
                "required": ["title", "html"]
            }),
        }];

        let mut called = Vec::new();
        let outcome = complete_with_tools_stream(
            &store,
            None,
            None,
            Some("Use tools."),
            &[("user".to_string(), "make a page".to_string())],
            &tools,
            4,
            |_| {},
            |req| {
                called.push((req.name.clone(), req.arguments.clone()));
                ToolExecResult {
                    content: json!({ "ok": true, "pageId": "page-saved-id" }).to_string(),
                    is_error: false,
                }
            },
        )
        .unwrap();

        assert_eq!(called.len(), 1);
        assert_eq!(called[0].0, "create_page");
        assert_eq!(called[0].1["title"], json!("Reading"));
        assert_eq!(outcome.text, "Created your page!");
        handle.join().unwrap();
    }
}



