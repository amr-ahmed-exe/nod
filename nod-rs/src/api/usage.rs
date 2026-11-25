//! Usage, spend limits, and activity ledger endpoints backed by SQLite records.

use super::{text, Ctx, Reply};
use crate::db::Store;
use crate::server::Live;
use crate::util::{days_in_month, iso, iso_now, now, start_of_month, uuid};
use chrono::{Datelike, Duration};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Records a completed or failed LLM call into the local usage & spending ledger.
#[allow(clippy::too_many_arguments)]
pub fn record_llm_usage(
    store: &Store,
    live: &Live,
    provider: &str,
    model_id: &str,
    source: &str,
    title: &str,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
    error: Option<&str>,
) {
    let id = uuid();
    let at = iso_now();
    let billing = if cost_usd <= 0.0 { "plan" } else { "api" };
    let clean_title: String = {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            "Chat message".to_string()
        } else {
            trimmed.chars().take(72).collect()
        }
    };
    let doc = json!({
        "id": id,
        "at": at,
        "title": clean_title,
        "source": source,
        "channel": "desktop",
        "provider": provider,
        "model": model_id,
        "billing": billing,
        "inputTokens": input_tokens,
        "outputTokens": output_tokens,
        "costUsd": cost_usd,
        "error": error,
        "steps": [{
            "at": at,
            "kind": "model",
            "title": format!("{} ({})", model_id, provider),
            "detail": format!("{input_tokens} in · {output_tokens} out tokens"),
            "costUsd": cost_usd,
            "failed": error.is_some(),
        }],
    });
    let _ = store.put("usage_entries", &id, &doc);
    live.notify("usage");
}

/// Handles `/api/trust*` and `/api/usage/work` routes.
pub fn handle(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    match (method, path) {
        ("GET", "/api/trust") => Some(trust(ctx)),
        ("GET", "/api/trust/entries") => Some(trust_entries(ctx)),
        ("PUT", "/api/trust/limit") => Some(trust_limit(ctx)),
        ("GET", "/api/trust/export") => Some(trust_export(ctx)),
        ("GET", "/api/usage/work") => Some(work_usage(ctx)),
        _ => {
            if let Some(id) = path
                .strip_prefix("/api/trust/entries/")
                .and_then(|s| s.strip_suffix("/steps"))
                && method == "GET"
            {
                return Some(entry_steps(ctx, id));
            }
            None
        }
    }
}

fn entry_view(row: &Value) -> Value {
    let cost = row.get("costUsd").and_then(Value::as_f64).unwrap_or(0.0);
    let provider = row
        .get("provider")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let model = row
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let billing = text(row, "billing");
    let billing = if billing == "plan" { "plan" } else { "api" };
    let mut obj = json!({
        "id": text(row, "id"),
        "messageId": Value::Null,
        "at": text(row, "at"),
        "title": text(row, "title"),
        "source": text(row, "source"),
        "channel": text(row, "channel"),
        "error": row.get("error").cloned().unwrap_or(Value::Null),
        "costUsd": if cost > 0.0 { json!(cost) } else { json!(0.0) },
        "models": [{
            "provider": provider,
            "model": model,
            "billing": billing,
        }],
        "actions": [],
    });
    if cost <= 0.0
        && let Some(map) = obj.as_object_mut()
    {
        map.insert("pricing".to_string(), json!("unpriced"));
    }
    obj
}

fn sorted_rows(store: &Store) -> Vec<Value> {
    let mut rows = store.all("usage_entries");
    rows.sort_by_key(|r| std::cmp::Reverse(text(r, "at")));
    rows
}

fn trust(ctx: &Ctx) -> Reply {
    let utc_now = now();
    let month_start = start_of_month(utc_now);
    let year = utc_now.year();
    let month = utc_now.month();
    let day = utc_now.day();
    let total_days = days_in_month(year, month);
    let mut daily: Vec<f64> = vec![0.0; total_days as usize];

    let month_start_iso = iso(month_start);
    let week_cutoff = (utc_now - Duration::days(7)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let month_prefix = format!("{year:04}-{month:02}-");

    let rows = sorted_rows(ctx.store);
    let mut week_things = 0_u64;
    let mut week_spent = 0.0_f64;
    let mut month_spent = 0.0_f64;
    let mut bg_spent = 0.0_f64;
    let mut unpriced = false;

    for row in &rows {
        let at = text(row, "at");
        let cost = row.get("costUsd").and_then(Value::as_f64).unwrap_or(0.0);
        if cost <= 0.0 {
            unpriced = true;
        }
        if at >= week_cutoff {
            week_things += 1;
            week_spent += cost;
        }
        if at.starts_with(&month_prefix) {
            month_spent += cost;
            if text(row, "source") != "chat" {
                bg_spent += cost;
            }
            if let Some(day_num) = at
                .get(8..10)
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|d| *d >= 1 && *d <= daily.len())
            {
                daily[day_num - 1] += cost;
            }
        }
    }

    let elapsed = (day as f64).max(1.0);
    let forecast = (month_spent / elapsed) * (total_days as f64);

    let limit_setting = ctx.store.setting("trust_limit");
    let limit = limit_setting.and_then(|val| {
        val.get("monthlyUsd").and_then(Value::as_f64).map(|monthly_usd| {
            json!({
                "monthlyUsd": monthly_usd,
                "reached": monthly_usd > 0.0 && month_spent >= monthly_usd
            })
        })
    });

    let entries: Vec<Value> = rows.iter().take(25).map(entry_view).collect();
    Reply::ok(json!({
        "entries": entries,
        "next": Value::Null,
        "week": {
            "things": week_things,
            "actions": 0,
            "spentUsd": week_spent,
            "planUsd": 0.0
        },
        "month": {
            "start": month_start_iso,
            "days": total_days,
            "elapsedDays": day,
            "spentUsd": month_spent,
            "forecastUsd": forecast,
            "planUsd": 0.0,
            "daily": daily,
            "unpriced": unpriced,
            "planUsage": false,
            "backgroundUsd": bg_spent,
            "outside": []
        },
        "limit": limit,
        "taskUsd": Value::Null
    }))
}

fn trust_entries(ctx: &Ctx) -> Reply {
    let entries: Vec<Value> = sorted_rows(ctx.store)
        .into_iter()
        .take(100)
        .map(|r| entry_view(&r))
        .collect();
    Reply::ok(json!({ "entries": entries, "next": Value::Null }))
}

fn entry_steps(ctx: &Ctx, id: &str) -> Reply {
    let Some(row) = ctx.store.get("usage_entries", id) else {
        return Reply::error(404, "Usage entry not found.");
    };
    let steps = row
        .get("steps")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total = steps.len();
    Reply::ok(json!({ "steps": steps, "total": total }))
}

fn trust_limit(ctx: &Ctx) -> Reply {
    if ctx.body.get("monthlyUsd").map(Value::is_null).unwrap_or(false) {
        ctx.store.set_setting("trust_limit", &Value::Null);
    } else if !ctx.body.is_null() {
        ctx.store.set_setting("trust_limit", &ctx.body);
    }
    ctx.live.notify("usage");
    ctx.refresh();
    Reply::ok(json!({ "ok": true }))
}

fn trust_export(ctx: &Ctx) -> Reply {
    let mut csv = String::from("at,title,source,provider,model,inputTokens,outputTokens,costUsd\n");
    for row in sorted_rows(ctx.store) {
        let title = text(&row, "title").replace('"', "\"\"");
        let at = text(&row, "at");
        let source = text(&row, "source");
        let provider = text(&row, "provider");
        let model = text(&row, "model");
        let inp = row.get("inputTokens").and_then(Value::as_u64).unwrap_or(0);
        let out = row.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
        let cost = row.get("costUsd").and_then(Value::as_f64).unwrap_or(0.0);
        csv.push_str(&format!("{at},\"{title}\",{source},{provider},{model},{inp},{out},{cost:.6}\n"));
    }
    Reply::text(200, csv, "text/csv; charset=utf-8")
}

fn build_lane(rows: &[Value], lane: &str) -> Value {
    let mut input = 0_u64;
    let mut output = 0_u64;
    let mut usd = 0.0_f64;
    let mut by_model: BTreeMap<(String, String), (u64, u64, f64)> = BTreeMap::new();

    for row in rows {
        let src = text(row, "source");
        let matches = match lane {
            "chat" => src == "chat",
            "schedule" => src == "schedule",
            _ => src != "chat" && src != "schedule",
        };
        if !matches {
            continue;
        }
        let inp = row.get("inputTokens").and_then(Value::as_u64).unwrap_or(0);
        let out = row.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
        let cost = row.get("costUsd").and_then(Value::as_f64).unwrap_or(0.0);
        input += inp;
        output += out;
        usd += cost;
        let entry = by_model
            .entry((text(row, "provider"), text(row, "model")))
            .or_insert((0, 0, 0.0));
        entry.0 += inp;
        entry.1 += out;
        entry.2 += cost;
    }

    let models: Vec<Value> = by_model
        .into_iter()
        .map(|((provider, model), (inp, out, cost))| {
            json!({
                "provider": provider,
                "model": model,
                "input": inp,
                "output": out,
                "usd": cost
            })
        })
        .collect();

    json!({
        "input": input,
        "output": output,
        "cacheRead": 0,
        "cacheWrite": 0,
        "agentUsd": usd,
        "otherUsd": 0.0,
        "models": models
    })
}

fn work_usage(ctx: &Ctx) -> Reply {
    let rows = ctx.store.all("usage_entries");
    Reply::ok(json!({
        "lanes": {
            "chat": build_lane(&rows, "chat"),
            "schedule": build_lane(&rows, "schedule"),
            "background": build_lane(&rows, "background")
        },
        "schedules": {},
        "tasks": {},
        "own": []
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeManager;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn usage_records_accumulate_across_trust_export_and_lanes() {
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let live = Arc::new(Live::default());
        let runtime = RuntimeManager::new(Arc::clone(&live));

        record_llm_usage(
            store.as_ref(),
            live.as_ref(),
            "openai",
            "gpt-4o-mini",
            "chat",
            "Say hello",
            120,
            45,
            0.0025,
            None,
        );

        let query = HashMap::new();
        let ctx = Ctx {
            method: "GET",
            path: "/api/trust",
            query: &query,
            body: Value::Null,
            body_bytes: &[],
            content_type: "application/json",
            store: &store,
            live: &live,
            runtime: &runtime,
            owner: "owner",
        };
        let reply = handle(&ctx).expect("handled");
        assert_eq!(reply.body["week"]["things"], 1);
        assert_eq!(reply.body["entries"].as_array().unwrap().len(), 1);

        let work_ctx = Ctx {
            path: "/api/usage/work",
            ..ctx
        };
        let work = handle(&work_ctx).expect("handled");
        assert_eq!(work.body["lanes"]["chat"]["input"], 120);
        assert_eq!(work.body["lanes"]["chat"]["output"], 45);
    }
}
