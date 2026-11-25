use super::{models_internal, text, Ctx, Reply};
use crate::db::Store;
use crate::util::{iso, iso_now, local_zone, parse_iso, uuid, zone, Cron};
use serde_json::{json, Value};

pub fn handle(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    if method == "GET" && path == "/api/schedules" {
        return Some(list(ctx));
    }
    if method == "POST" && path == "/api/schedules" {
        return Some(create(ctx));
    }
    if method == "GET" && path == "/api/schedules/ideas" {
        return Some(ideas(ctx));
    }
    if method == "GET" && path == "/api/schedule-runs" {
        return Some(runs(ctx));
    }
    if let Some(rest) = path.strip_prefix("/api/schedules/") {
        return Some(by_id(ctx, rest));
    }
    None
}

fn list(ctx: &Ctx) -> Reply {
    let schedules = ctx.store.all("schedules");
    let mut views: Vec<Value> = schedules.iter().map(schedule_view).collect();
    views.sort_by(|a, b| {
        let a_next = a.get("nextRun").and_then(Value::as_str).unwrap_or("");
        let b_next = b.get("nextRun").and_then(Value::as_str).unwrap_or("");
        a_next.cmp(b_next)
    });
    Reply::ok(Value::Array(views))
}

fn is_valid_uuid_v4_like(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes()[8] == b'-'
        && s.as_bytes()[13] == b'-'
        && s.as_bytes()[18] == b'-'
        && s.as_bytes()[23] == b'-'
        && s.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
}

fn create(ctx: &Ctx) -> Reply {
    let title = text(&ctx.body, "title").trim().to_string();
    if title.is_empty() {
        return Reply::error(400, "Enter a schedule title.");
    }
    let prompt = text(&ctx.body, "prompt").trim().to_string();
    if prompt.is_empty() {
        return Reply::error(400, "Enter a schedule instruction.");
    }
    let kind = ctx
        .body
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("recurring");
    if kind != "once" && kind != "recurring" {
        return Reply::error(400, "Choose a schedule kind of once or recurring.");
    }
    let tz_name = ctx
        .body
        .get("timeZone")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(local_zone().name());
    let cron_expr = text(&ctx.body, "cron").trim().to_string();
    let run_at = ctx.body.get("runAt").cloned().unwrap_or(Value::Null);
    if kind == "once" {
        let Some(ts) = run_at.as_str().filter(|s| !s.trim().is_empty()) else {
            return Reply::error(400, "Choose when the one-time schedule runs.");
        };
        if parse_iso(ts).is_none() {
            return Reply::error(400, "Enter a valid date and time for runAt.");
        }
    } else if cron_expr.is_empty() || Cron::parse(&cron_expr).is_none() {
        return Reply::error(400, "Enter a valid cron expression.");
    }
    let model = ctx.body.get("model").cloned().unwrap_or(Value::Null);
    let watch = if kind == "once" {
        Value::Null
    } else if let Some(arr) = ctx.body.get("watch").and_then(Value::as_array) {
        if arr.is_empty() || arr.len() > 10 {
            return Reply::error(400, "Watch accepts between 1 and 10 connection IDs.");
        }
        let mut clean = Vec::new();
        for item in arr {
            let Some(cid) = item.as_str().filter(|s| is_valid_uuid_v4_like(s)) else {
                return Reply::error(400, "Watched connections must be valid IDs.");
            };
            clean.push(json!(cid));
        }
        Value::Array(clean)
    } else {
        Value::Null
    };

    let id = uuid();
    let next_run = if kind == "once" {
        run_at.as_str().map(|s| s.to_string())
    } else {
        compute_next(&cron_expr, tz_name)
    };

    let schedule = json!({
        "id": id,
        "title": title,
        "prompt": prompt,
        "kind": kind,
        "cron": if kind == "once" { Value::Null } else { json!(cron_expr) },
        "runAt": run_at,
        "timeZone": tz_name,
        "enabled": true,
        "revision": 1,
        "nextRun": next_run,
        "lastRun": null,
        "activeRun": null,
        "baselineObservation": null,
        "watch": watch,
        "model": model,
        "monthlyUsd": null,
        "createdAt": iso_now()
    });
    let _ = ctx.store.put("schedules", &id, &schedule);
    ctx.refresh();
    Reply::ok(schedule_view(&schedule))
}

fn by_id(ctx: &Ctx, rest: &str) -> Reply {
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let id = parts.first().copied().unwrap_or("");
    let action = parts.get(1).copied();

    let Some(schedule) = ctx.store.get("schedules", id) else {
        return Reply::error(404, "Schedule not found.");
    };

    match (ctx.method, action, parts.get(2).copied(), parts.get(3).copied()) {
        ("GET", None, _, _) => Reply::ok(schedule_view(&schedule)),
        ("PATCH", None, _, _) => {
            let Some(updated) = ctx.store.patch("schedules", id, &ctx.body) else {
                return Reply::error(404, "Schedule not found.");
            };
            let mut s = updated;
            if let Some(obj) = s.as_object_mut() {
                let rev = obj.get("revision").and_then(Value::as_i64).unwrap_or(0);
                obj.insert("revision".to_string(), json!(rev + 1));
                let kind = obj
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("recurring")
                    .to_string();
                if kind == "once" {
                    obj.insert("watch".to_string(), Value::Null);
                    obj.insert("cron".to_string(), Value::Null);
                    if let Some(run_at) = obj.get("runAt").cloned() {
                        obj.insert("nextRun".to_string(), run_at);
                    }
                } else {
                    let cron = obj
                        .get("cron")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let tz = obj
                        .get("timeZone")
                        .and_then(Value::as_str)
                        .unwrap_or("UTC")
                        .to_string();
                    if !cron.is_empty()
                        && let Some(next) = compute_next(&cron, &tz)
                    {
                        obj.insert("nextRun".to_string(), json!(next));
                    }
                }
                obj.remove("expectedRevision");
            }
            let _ = ctx.store.put("schedules", id, &s);
            ctx.refresh();
            Reply::ok(schedule_view(&s))
        }
        ("PUT", Some("model"), _, _) => {
            let model = ctx.body.get("model").cloned().unwrap_or(Value::Null);
            let mut s = schedule;
            if let Some(obj) = s.as_object_mut() {
                obj.insert("model".to_string(), model);
                let rev = obj.get("revision").and_then(Value::as_i64).unwrap_or(0);
                obj.insert("revision".to_string(), json!(rev + 1));
            }
            let _ = ctx.store.put("schedules", id, &s);
            ctx.refresh();
            Reply::ok(schedule_view(&s))
        }
        ("POST", Some("preview"), _, _) => {
            let kind = ctx
                .body
                .get("kind")
                .and_then(Value::as_str)
                .or_else(|| schedule.get("kind").and_then(Value::as_str))
                .unwrap_or("recurring");
            if kind == "once" {
                let run_at = ctx
                    .body
                    .get("runAt")
                    .and_then(Value::as_str)
                    .or_else(|| schedule.get("runAt").and_then(Value::as_str));
                let next = run_at.filter(|ts| {
                    parse_iso(ts)
                        .map(|dt| dt > chrono::Utc::now())
                        .unwrap_or(false)
                });
                return Reply::ok(json!({ "nextRun": next }));
            }
            let cron_expr = ctx
                .body
                .get("cron")
                .and_then(Value::as_str)
                .or_else(|| schedule.get("cron").and_then(Value::as_str))
                .unwrap_or_default();
            let tz_name = ctx
                .body
                .get("timeZone")
                .and_then(Value::as_str)
                .or_else(|| schedule.get("timeZone").and_then(Value::as_str))
                .unwrap_or("UTC");
            Reply::ok(json!({ "nextRun": compute_next(cron_expr, tz_name) }))
        }
        ("DELETE", None, _, _) => {
            ctx.store.delete("schedules", id);
            ctx.refresh();
            Reply::ok(json!({ "ok": true }))
        }
        ("POST", Some("run"), _, _) => start_schedule_run(ctx, id, schedule),
        ("POST", Some("stop"), _, _) => {
            let turn_id = schedule
                .pointer("/activeRun/turnId")
                .and_then(Value::as_str)
                .map(str::to_string);
            stop_schedule_run(ctx, id, turn_id.as_deref())
        }
        ("POST", Some("runs"), Some(turn_id), Some("stop")) => {
            stop_schedule_run(ctx, id, Some(turn_id))
        }
        _ => Reply::error(404, "Schedule endpoint not found."),
    }
}

fn resolve_schedule_model(store: &Store, schedule: &Value) -> Option<Value> {
    if let Some(model) = schedule.get("model").filter(|v| !v.is_null()) {
        return Some(model.clone());
    }
    let state = models_internal(store);
    if let Some(route) = state.pointer("/routes/schedule").filter(|v| !v.is_null()) {
        if route.as_str() == Some("light") {
            if let Some(light) = state.get("light").filter(|v| !v.is_null()) {
                return Some(light.clone());
            }
        } else {
            return Some(route.clone());
        }
    }
    None
}

fn start_schedule_run(ctx: &Ctx, id: &str, mut s: Value) -> Reply {
    let turn_id = uuid();
    let run_id = uuid();
    let now_str = iso_now();
    let title = text(&s, "title");
    let prompt = text(&s, "prompt");
    let tz = s
        .get("timeZone")
        .and_then(Value::as_str)
        .unwrap_or("UTC")
        .to_string();
    let active = json!({
        "turnId": turn_id,
        "status": "running",
        "questionMessageId": null
    });
    if let Some(obj) = s.as_object_mut() {
        obj.insert("activeRun".to_string(), active.clone());
    }
    let _ = ctx.store.put("schedules", id, &s);

    let initial_run = json!({
        "id": run_id,
        "scheduleId": id,
        "title": title,
        "turnId": turn_id,
        "scheduledFor": now_str,
        "timeZone": tz,
        "createdAt": now_str,
        "status": "running",
        "error": null,
        "observation": null,
        "messageId": null,
        "messagePreview": null,
        "messageText": null,
        "attachmentMessageId": null,
        "costUsd": null
    });
    let _ = ctx.store.put("schedule-runs", &run_id, &initial_run);
    ctx.refresh();

    let store = ctx.store.clone();
    let live = ctx.live.clone();
    let sid = id.to_string();
    let sched_model = resolve_schedule_model(&store, &s);
    let worker_turn = turn_id.clone();
    std::thread::spawn(move || {
        let model_res = if store.exists("stopped-turns", &worker_turn) {
            Err("Stopped.".to_string())
        } else {
            let sys = format!(
                "You are running the scheduled task \"{title}\". Execute or answer concisely and clearly. \
                If the task creates or updates a Page, call `create_page` with complete self-contained HTML or output <nod-page> HTML. \
                A scheduled run cannot create schedules."
            );
            let history = vec![("user".to_string(), prompt.clone())];
            match super::chat::run_agent_with_tools(
                store.as_ref(),
                &live,
                super::bots::MAIN_BOT_ID,
                true,
                Some(&sid),
                &prompt,
                sched_model.as_ref(),
                None,
                Some(&sys),
                &history,
                |_| {},
            ) {
                Ok(agent_run) => {
                    let out = agent_run.outcome;
                    super::usage::record_llm_usage(
                        store.as_ref(),
                        &live,
                        &out.provider,
                        &out.model_id,
                        "schedule",
                        &title,
                        out.input_tokens,
                        out.output_tokens,
                        out.cost_usd,
                        None,
                    );
                    let (clean, _artifacts, saved_pages) = super::pages::extract_and_save_pages(
                        store.as_ref(),
                        &live,
                        &out.text,
                        true,
                        Some(&sid),
                    );
                    let trimmed = clean.trim();
                    if trimmed.is_empty() && !saved_pages.is_empty() {
                        let titles: Vec<String> = saved_pages
                            .iter()
                            .map(|p| text(p, "title"))
                            .collect();
                        Ok(format!("Updated page(s): {}", titles.join(", ")))
                    } else if trimmed.is_empty() && !agent_run.notes.is_empty() {
                        Ok(agent_run.notes.join("; "))
                    } else {
                        Ok(trimmed.to_string())
                    }
                }
                Err(err) if err == crate::llm::DEMO => Ok(crate::llm::demo_reply(&prompt)),
                Err(err) => Err(err),
            }
        };

        let cancelled = store.exists("stopped-turns", &worker_turn);
        let finished_status = if cancelled {
            "cancelled"
        } else if model_res.is_ok() {
            "succeeded"
        } else {
            "failed"
        };
        let (msg_text, err_text) = match model_res {
            Ok(txt) if !cancelled => (Some(txt), None),
            Ok(_) => (None, Some("Stopped.".to_string())),
            Err(err) => (None, Some(err)),
        };
        let preview = msg_text
            .as_ref()
            .map(|t| t.chars().take(160).collect::<String>());
        let final_run = json!({
            "id": run_id,
            "scheduleId": sid,
            "title": title,
            "turnId": worker_turn,
            "scheduledFor": now_str,
            "timeZone": tz,
            "createdAt": now_str,
            "status": finished_status,
            "error": err_text,
            "observation": null,
            "messageId": null,
            "messagePreview": preview,
            "messageText": msg_text,
            "attachmentMessageId": null,
            "costUsd": null
        });
        let _ = store.put("schedule-runs", &run_id, &final_run);
        if let Some(mut updated_sched) = store.get("schedules", &sid) {
            if let Some(obj) = updated_sched.as_object_mut() {
                obj.insert("activeRun".to_string(), Value::Null);
                obj.insert("lastRun".to_string(), final_run);
                let rev = obj.get("revision").and_then(Value::as_i64).unwrap_or(0);
                obj.insert("revision".to_string(), json!(rev + 1));
            }
            let _ = store.put("schedules", &sid, &updated_sched);
        }
        live.broadcast(&json!({ "type": "refresh" }));
    });

    Reply::ok(json!({
        "turnId": turn_id,
        "activeRun": active
    }))
}

fn stop_schedule_run(ctx: &Ctx, id: &str, turn_id: Option<&str>) -> Reply {
    if let Some(tid) = turn_id {
        let _ = ctx.store.put("stopped-turns", tid, &json!({ "at": iso_now() }));
        for mut run in ctx.store.all("schedule-runs") {
            if run.get("turnId").and_then(Value::as_str) == Some(tid)
                && let Some(rid) = run.get("id").and_then(Value::as_str).map(str::to_string)
            {
                if let Some(obj) = run.as_object_mut() {
                    obj.insert("status".to_string(), json!("cancelled"));
                    obj.insert("error".to_string(), json!("Stopped."));
                }
                let _ = ctx.store.put("schedule-runs", &rid, &run);
            }
        }
    }
    if let Some(mut s) = ctx.store.get("schedules", id) {
        if let Some(obj) = s.as_object_mut() {
            obj.insert("activeRun".to_string(), Value::Null);
            let rev = obj.get("revision").and_then(Value::as_i64).unwrap_or(0);
            obj.insert("revision".to_string(), json!(rev + 1));
        }
        let _ = ctx.store.put("schedules", id, &s);
    }
    ctx.refresh();
    Reply::ok(json!({ "ok": true }))
}

fn runs(ctx: &Ctx) -> Reply {
    let filter_schedule = ctx.param("scheduleId");
    let limit = ctx
        .param("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(50)
        .clamp(1, 100);
    let mut rows: Vec<Value> = ctx
        .store
        .all("schedule-runs")
        .into_iter()
        .filter(|run| {
            filter_schedule
                .as_deref()
                .map(|sid| run.get("scheduleId").and_then(Value::as_str) == Some(sid))
                .unwrap_or(true)
        })
        .collect();
    rows.sort_by_key(|a| std::cmp::Reverse(text(a, "createdAt")));
    rows.truncate(limit);
    Reply::ok(json!({
        "runs": rows,
        "nextCursor": null
    }))
}

/// Suggests personalized recurring schedule ideas grounded in recent user messages (`#1444`).
fn ideas(ctx: &Ctx) -> Reply {
    let mem_cfg = ctx.store.setting("memory_settings").unwrap_or_else(|| json!({}));
    let saving = mem_cfg.get("saving").and_then(Value::as_bool).unwrap_or(true);
    let allow_sensitive = mem_cfg.get("sensitive").and_then(Value::as_bool).unwrap_or(false);
    if !saving {
        return Reply::ok(json!({ "ideas": [] }));
    }

    let existing_titles: Vec<String> = ctx
        .store
        .all("schedules")
        .iter()
        .map(|s| text(s, "title").to_lowercase())
        .collect();

    let user_msgs: Vec<String> = ctx
        .store
        .all("messages")
        .into_iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|m| text(&m, "text"))
        .filter(|t| !t.trim().is_empty())
        .collect();

    if user_msgs.is_empty() {
        return Reply::ok(json!({ "ideas": [] }));
    }

    if let Some(cached) = ctx.store.setting("schedule_ideas")
        && let Some(cached_count) = cached.get("msgCount").and_then(Value::as_u64)
        && cached_count as usize == user_msgs.len()
        && let Some(items) = cached.get("ideas").and_then(Value::as_array)
    {
        let filtered: Vec<Value> = items
            .iter()
            .filter(|item| {
                let title = text(item, "title").to_lowercase();
                let sens = item.get("sensitive").and_then(Value::as_bool).unwrap_or(false);
                (allow_sensitive || !sens) && !existing_titles.contains(&title)
            })
            .cloned()
            .collect();
        return Reply::ok(json!({ "ideas": filtered }));
    }

    let recent: Vec<String> = user_msgs.iter().rev().take(15).cloned().collect();
    let msg_count = user_msgs.len();
    let apps: Vec<String> = ctx
        .store
        .all("apps")
        .iter()
        .filter(|a| a.get("status").and_then(Value::as_str) == Some("connected"))
        .map(|a| text(a, "slug"))
        .filter(|s| !s.is_empty())
        .collect();

    // Try curating via configured LLM; gracefully return empty or matched ideas without blocking UI.
    let prompt_json = json!({
        "messages": recent.into_iter().rev().collect::<Vec<_>>(),
        "schedules": existing_titles,
        "apps": apps
    });
    let sys = "Suggest up to 3 recurring personal assistant schedule ideas grounded in the user's messages. Reply with JSON only: {\"ideas\":[{\"title\":string,\"when\":string,\"sensitive\":boolean}]}. Keep titles under 10 words starting with a verb.";
    let mut curated = Vec::new();
    if let Ok(out) = crate::llm::complete_with_selection(
        ctx.store.as_ref(),
        None,
        None,
        Some(sys),
        &[("user".to_string(), prompt_json.to_string())],
    ) && let Some(start) = out.text.find('{')
        && let Some(end) = out.text.rfind('}')
        && let Ok(parsed) = serde_json::from_str::<Value>(&out.text[start..=end])
        && let Some(items) = parsed.get("ideas").and_then(Value::as_array)
    {
        for item in items.iter().take(3) {
            let title = text(item, "title");
            let when = text(item, "when");
            let sensitive = item.get("sensitive").and_then(Value::as_bool).unwrap_or(false);
            if !title.is_empty()
                && !when.is_empty()
                && !existing_titles.contains(&title.to_lowercase())
                && (allow_sensitive || !sensitive)
            {
                let mut entry = json!({ "title": title, "when": when });
                if sensitive {
                    entry["sensitive"] = json!(true);
                }
                curated.push(entry);
            }
        }
    }

    ctx.store.set_setting(
        "schedule_ideas",
        &json!({ "msgCount": msg_count, "ideas": curated }),
    );
    Reply::ok(json!({ "ideas": curated }))
}

/// Creates or updates a recurring schedule triggered from an assistant chat turn.
pub fn create_from_chat(
    store: &Store,
    live: &crate::server::Live,
    raw_title: &str,
    raw_cron: &str,
    raw_prompt: &str,
) -> Option<Value> {
    let title = raw_title.trim();
    let prompt = raw_prompt.trim();
    if title.is_empty() || prompt.is_empty() {
        return None;
    }
    let cron_expr = if Cron::parse(raw_cron.trim()).is_some() {
        raw_cron.trim()
    } else {
        "0 8 * * *"
    };
    let tz_name = local_zone().name();
    let now = iso_now();
    let next_run = compute_next(cron_expr, tz_name);

    if let Some(mut existing) = store
        .all("schedules")
        .into_iter()
        .find(|s| text(s, "title").eq_ignore_ascii_case(title))
    {
        let id = text(&existing, "id");
        if let Some(obj) = existing.as_object_mut() {
            obj.insert("prompt".to_string(), json!(prompt));
            obj.insert("cron".to_string(), json!(cron_expr));
            obj.insert("timeZone".to_string(), json!(tz_name));
            obj.insert("enabled".to_string(), json!(true));
            obj.insert("nextRun".to_string(), json!(next_run));
            let rev = obj.get("revision").and_then(Value::as_i64).unwrap_or(1);
            obj.insert("revision".to_string(), json!(rev + 1));
        }
        let _ = store.put("schedules", &id, &existing);
        live.broadcast(&json!({ "type": "refresh" }));
        return Some(schedule_view(&existing));
    }

    let id = uuid();
    let schedule = json!({
        "id": id,
        "title": title,
        "prompt": prompt,
        "kind": "recurring",
        "cron": cron_expr,
        "runAt": Value::Null,
        "timeZone": tz_name,
        "enabled": true,
        "revision": 1,
        "nextRun": next_run,
        "lastRun": Value::Null,
        "activeRun": Value::Null,
        "baselineObservation": Value::Null,
        "watch": Value::Null,
        "model": Value::Null,
        "monthlyUsd": Value::Null,
        "createdAt": now
    });
    let _ = store.put("schedules", &id, &schedule);
    live.broadcast(&json!({ "type": "refresh" }));
    Some(schedule_view(&schedule))
}

fn schedule_view(schedule: &Value) -> Value {
    schedule.clone()
}

fn compute_next(cron_expr: &str, tz_name: &str) -> Option<String> {
    let cron = Cron::parse(cron_expr)?;
    let tz = zone(tz_name);
    let now_utc = chrono::Utc::now();
    let runs = cron.next_runs(now_utc, tz, 1);
    runs.first().map(|dt| iso(*dt))
}
