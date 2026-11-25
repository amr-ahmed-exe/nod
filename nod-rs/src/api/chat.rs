use super::{bots, configuration_for_bot, flag, provider_name, tasks, text, Ctx, Reply};
use crate::db::Store;
use crate::llm::StreamEvent;
use crate::server::Live;
use crate::util::{iso, iso_now, parse_iso, uuid};
use chrono::Duration;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

static ACTIVE_ACTIVITIES: OnceLock<Mutex<HashMap<String, Vec<Value>>>> = OnceLock::new();

fn active_activities() -> &'static Mutex<HashMap<String, Vec<Value>>> {
    ACTIVE_ACTIVITIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn set_thread_activity(live: &Live, thread_id: &str, items: Vec<Value>) {
    if let Ok(mut guard) = active_activities().lock() {
        if items.is_empty() {
            guard.remove(thread_id);
        } else {
            guard.insert(thread_id.to_string(), items.clone());
        }
    }
    live.broadcast(&json!({ "type": "activity", "items": items }));
}

fn thread_activity(thread_id: &str) -> Vec<Value> {
    active_activities()
        .lock()
        .ok()
        .and_then(|guard| guard.get(thread_id).cloned())
        .unwrap_or_default()
}

pub fn handle(ctx: &Ctx) -> Option<Reply> {
    match (ctx.method, ctx.path) {
        ("GET", "/api/state") => Some(state_for_room(ctx, false)),
        ("GET", "/api/group/state") => Some(state_for_room(ctx, true)),
        ("GET", "/api/messages") => Some(list(ctx)),
        ("GET", "/api/messages/search") => Some(search(ctx)),
        ("POST", "/api/messages") => Some(send_for_room(ctx, false)),
        ("POST", "/api/group/messages") => Some(send_for_room(ctx, true)),
        ("POST", "/api/thread/stop") => Some(stop_thread(ctx)),
        ("POST", "/api/conversation/reset") => Some(reset(ctx)),
        ("POST", "/api/conversation/restart-context") => Some(restart(ctx)),
        _ => {
            if ctx.path.starts_with("/api/messages/") {
                let rest = ctx.path.trim_start_matches("/api/messages/");
                if let Some(id) = rest.strip_suffix("/reaction")
                    && ctx.method == "POST" {
                        return Some(reaction(ctx, id));
                    }
                if let Some(id) = rest.strip_prefix("around/")
                    && ctx.method == "GET" {
                        return Some(around(ctx, id));
                    }
            }
            None
        }
    }
}

fn non_empty_field(body: &Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn room_for_ctx(ctx: &Ctx, force_group: bool) -> bots::ActiveRoom {
    let req_bot = ctx.param("botId").or_else(|| non_empty_field(&ctx.body, "botId"));
    let req_room = if force_group {
        Some("group".to_string())
    } else {
        ctx.param("room").or_else(|| non_empty_field(&ctx.body, "room"))
    };
    bots::active_room_context(ctx.store, req_bot.as_deref(), req_room.as_deref())
}

pub fn current_thread(store: &Store) -> String {
    if let Some(thread) = store.setting("thread")
        && let Some(id) = thread.get("id").and_then(Value::as_str)
            && !id.is_empty() {
                return id.to_string();
            }
    new_thread(store)
}

pub fn new_thread(store: &Store) -> String {
    let id = uuid();
    store.set_setting("thread", &json!({ "id": id, "createdAt": iso_now() }));
    id
}

pub fn messages(store: &Store, thread: &str) -> Vec<Value> {
    store
        .all("messages")
        .into_iter()
        .filter(|message| {
            message.get("threadId").and_then(Value::as_str) == Some(thread)
                && message
                    .get("hiddenAt")
                    .map(Value::is_null)
                    .unwrap_or(true)
        })
        .collect()
}

pub fn message_view(message: &Value) -> Value {
    let mut view = message.clone();
    if let Some(object) = view.as_object_mut() {
        object.remove("threadId");
        object.remove("hiddenAt");
        object.remove("demo");
        object.remove("snippet");
        if !object.contains_key("attachments") || object["attachments"].is_null() {
            object.insert("attachments".to_string(), json!([]));
        }
        if !object.contains_key("reaction") {
            object.insert("reaction".to_string(), Value::Null);
        }
    }
    view
}

pub fn turns(store: &Store, thread: &str) -> Vec<Value> {
    let mut rows: Vec<Value> = store
        .all("turns")
        .into_iter()
        .filter(|turn| turn.get("threadId").and_then(Value::as_str) == Some(thread))
        .collect();
    rows.reverse();
    rows.truncate(50);
    rows
}

fn state_for_room(ctx: &Ctx, force_group: bool) -> Reply {
    let room = room_for_ctx(ctx, force_group);
    let messages: Vec<Value> = messages(ctx.store, &room.thread_id)
        .iter()
        .map(message_view)
        .collect();
    let cfg_bot = if room.is_group {
        None
    } else {
        Some(room.bot_id.as_str())
    };
    Reply::ok(json!({
        "threadId": room.thread_id,
        "botId": room.bot_id,
        "room": if room.is_group { "group" } else { "1on1" },
        "messages": messages,
        "turns": turns(ctx.store, &room.thread_id),
        "activity": thread_activity(&room.thread_id),
        "tasks": tasks::thread_tasks(ctx.store, &room.thread_id),
        "hasEarlierMessages": false,
        "hiddenMessageCount": 0,
        "hiddenMessageIds": [],
        "pendingAnswerChoiceMessageIds": [],
        "planningTasks": false,
        "configuration": configuration_for_bot(ctx.store, ctx.runtime.is_ready(), cfg_bot)
    }))
}

fn list(ctx: &Ctx) -> Reply {
    let room = room_for_ctx(ctx, false);
    let thread = room.thread_id;
    if let Some(requested) = ctx.param("threadId")
        && requested != thread {
            return Reply::error(
                409,
                "This conversation has been reset. Reload it before continuing.",
            );
        }
    let task = ctx.param("task");
    let mut rows: Vec<Value> = messages(ctx.store, &thread)
        .into_iter()
        .filter(|message| match task.as_deref() {
            Some("main") | None => message.get("taskId").map(Value::is_null).unwrap_or(true),
            Some(id) => message.get("taskId").and_then(Value::as_str) == Some(id),
        })
        .collect();
    rows.sort_by_key(|message| {
        (
            text(message, "createdAt"),
            text(message, "id"),
        )
    });
    if let Some(before) = ctx.param("before") {
        let before_id = ctx.param("beforeId");
        rows.retain(|message| {
            let at = text(message, "createdAt");
            if at == before {
                before_id
                    .as_deref()
                    .map(|id| text(message, "id").as_str() < id)
                    .unwrap_or(true)
            } else {
                at < before
            }
        });
    }
    let has_more = rows.len() > 60;
    if has_more {
        rows.drain(0..rows.len() - 60);
    }
    let views: Vec<Value> = rows.iter().map(message_view).collect();
    Reply::ok(json!({ "messages": views, "hasMore": has_more }))
}

fn search(ctx: &Ctx) -> Reply {
    let query = ctx.param("q").unwrap_or_default().trim().to_string();
    if query.is_empty() {
        return Reply::error(400, "Type something to search for.");
    }
    let thread = room_for_ctx(ctx, false).thread_id;
    let needle = query.to_lowercase();
    let mut views: Vec<Value> = Vec::new();
    for message in messages(ctx.store, &thread).iter().rev() {
        let body = text(message, "text");
        if body.to_lowercase().contains(&needle) {
            let mut view = message_view(message);
            view["snippet"] = json!(snippet(&body, &query));
            views.push(view);
            if views.len() >= 50 {
                break;
            }
        }
    }
    Reply::ok(Value::Array(views))
}

fn snippet(body: &str, query: &str) -> String {
    let lower = body.to_lowercase();
    let needle = query.to_lowercase();
    let at = lower.find(&needle).unwrap_or(0);
    let before: String = body[..at].chars().collect();
    let start = before.chars().count().saturating_sub(60);
    body.chars().skip(start).take(220).collect()
}

fn around(ctx: &Ctx, id: &str) -> Reply {
    let target = match ctx.store.get("messages", id) {
        Some(message) => message,
        None => return Reply::error(404, "Message not found."),
    };
    let hidden = target
        .get("hiddenAt")
        .map(|value| !value.is_null())
        .unwrap_or(false);
    if hidden {
        return Reply::error(404, "Message not found.");
    }
    let thread = text(&target, "threadId");
    let mut rows = messages(ctx.store, &thread);
    let target_at = text(&target, "createdAt");
    rows.sort_by_key(|message| text(message, "createdAt"));
    if let Some(instant) = parse_iso(&target_at) {
        let start = instant - Duration::days(1);
        let end = instant + Duration::days(1);
        let start_text = iso(start);
        let end_text = iso(end);
        let mut nearby: Vec<Value> = rows
            .iter()
            .filter(|message| {
                let at = text(message, "createdAt");
                at >= start_text && at <= end_text
            })
            .cloned()
            .collect();
        if nearby.len() > 60 {
            let target_key = text(&target, "id");
            let target_ms = instant.timestamp_millis();
            nearby.sort_by_key(|message| {
                let offset = parse_iso(&text(message, "createdAt"))
                    .map(|value| (value.timestamp_millis() - target_ms).abs())
                    .unwrap_or(i64::MAX);
                (offset, text(message, "id") != target_key)
            });
            nearby.truncate(60);
            nearby.sort_by_key(|message| text(message, "createdAt"));
        }
        return Reply::ok(Value::Array(nearby.iter().map(message_view).collect()));
    }
    let views: Vec<Value> = rows.iter().take(60).map(message_view).collect();
    Reply::ok(Value::Array(views))
}

fn send_for_room(ctx: &Ctx, force_group: bool) -> Reply {
    let message_text = super::text(&ctx.body, "text").trim().to_string();
    let display_text = super::text(&ctx.body, "displayText");
    let attachment_ids: Vec<String> = ctx
        .body
        .get("attachmentIds")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let attachments: Vec<Value> = attachment_ids
        .iter()
        .filter_map(|id| ctx.store.get("artifacts", id))
        .map(|artifact| artifact_view(&artifact))
        .collect();
    if message_text.is_empty() && attachments.is_empty() {
        return Reply::error(400, "Type a message first.");
    }

    let message_id = super::text(&ctx.body, "clientMessageId");
    let message_id = if message_id.trim().is_empty() {
        uuid()
    } else {
        message_id
    };
    if let Some(existing) = ctx.store.get("messages", &message_id) {
        return Reply::ok(json!({
            "threadId": super::text(&existing, "threadId"),
            "turnId": super::text(&existing, "turnId"),
            "messageId": message_id,
            "messageText": super::text(&existing, "text"),
            "attachments": existing.get("attachments").cloned().unwrap_or(json!([]))
        }));
    }

    let clear = message_text.eq_ignore_ascii_case("/clear")
        && attachments.is_empty()
        && super::text(&ctx.body, "displayText").is_empty();
    let new_chat = flag(&ctx.body, "newChat", false) || clear;
    let room = room_for_ctx(ctx, force_group);
    let current = room.thread_id.clone();
    let requested = super::text(&ctx.body, "threadId");
    if !new_chat && !requested.is_empty() && requested != current {
        return Reply::error(
            409,
            "This conversation has been reset. Reload it before continuing.",
        );
    }
    let thread = if new_chat {
        let fresh = new_thread(ctx.store);
        if room.is_group {
            bots::set_group_thread(ctx.store, &fresh);
        } else {
            bots::set_bot_thread(ctx.store, &room.bot_id, &fresh);
        }
        fresh
    } else {
        current
    };
    let explicit_target = non_empty_field(&ctx.body, "targetBotId")
        .or_else(|| if room.is_group { non_empty_field(&ctx.body, "botId") } else { None });
    let responding_bot = if room.is_group {
        bots::resolve_group_target_bot(ctx.store, &message_text, explicit_target.as_deref())
    } else {
        bots::get_bot(ctx.store, &room.bot_id).unwrap_or_else(|| {
            json!({ "id": bots::MAIN_BOT_ID, "name": "Main Bot", "handle": "main", "role": "main" })
        })
    };
    let target_bot_id = text(&responding_bot, "id");
    let target_bot_name = text(&responding_bot, "name");
    let target_bot_role = text(&responding_bot, "role");
    let turn_id = uuid();
    let now = iso_now();

    let reply_id = super::text(&ctx.body, "replyToMessageId");
    let reply_quote = super::text(&ctx.body, "replyQuote");
    let mut reply = Value::Null;
    if !reply_id.is_empty() {
        let Some(target) = ctx.store.get("messages", &reply_id) else {
            return Reply::error(404, "The reply is unavailable in this conversation.");
        };
        if text(&target, "threadId") != thread {
            return Reply::error(404, "The reply is unavailable in this conversation.");
        }
        let mut quote = json!({
            "id": reply_id,
            "role": text(&target, "role"),
            "text": if reply_quote.is_empty() {
                text(&target, "text")
            } else {
                reply_quote
            }
        });
        if let Some(value) = ctx.body.get("replyQuote").and_then(Value::as_str)
            && !value.is_empty() {
                quote["isQuote"] = json!(true);
            }
        reply = quote;
    }

    let mut user = json!({
        "id": message_id.clone(),
        "threadId": thread.clone(),
        "role": "user",
        "text": message_text.clone(),
        "turnId": turn_id.clone(),
        "createdAt": now.clone(),
        "attachments": attachments,
        "reaction": Value::Null,
        "botId": if room.is_group { Value::Null } else { json!(target_bot_id.clone()) },
        "targetBotId": if room.is_group { json!(target_bot_id.clone()) } else { Value::Null }
    });
    if !display_text.is_empty() && display_text != message_text {
        user["displayText"] = json!(display_text);
    }
    if !reply_id.is_empty() {
        user["replyToMessageId"] = json!(reply_id);
        if !reply.is_null() {
            user["replyTo"] = reply;
        }
    }
    if ctx.body.get("questionAnswer").is_some() || ctx.body.get("approvalAnswer").is_some() {
        user["cardAnswer"] = json!(true);
    }

    if clear {
        let notice = "Started a fresh chat. Your tasks, files, accounts and memory are still here. Previous messages are available in search.";
        let assistant_id = uuid();
        if let Err(error) = ctx.store.put("messages", &message_id, &user) {
            return Reply::error(500, error);
        }
        let assistant = json!({
            "id": assistant_id,
            "threadId": thread,
            "role": "assistant",
            "text": notice,
            "turnId": turn_id,
            "createdAt": iso_now(),
            "attachments": []
        });
        if let Err(error) = ctx.store.put("messages", &assistant_id, &assistant) {
            return Reply::error(500, error);
        }
        let turn = json!({
            "id": turn_id.clone(),
            "threadId": thread.clone(),
            "status": "succeeded",
            "source": "user",
            "createdAt": now.clone(),
            "finishedAt": iso_now(),
            "error": Value::Null
        });
        let _ = ctx.store.put("turns", &turn_id, &turn);
        ctx.refresh();
        return Reply::ok(json!({
            "threadId": thread,
            "turnId": turn_id,
            "messageId": message_id,
            "messageText": message_text,
            "attachments": []
        }));
    }

    let prompt = user_prompt(&message_text, &attachments);
    let history = if room.is_group {
        bots::visible_group_history(
            ctx.store.as_ref(),
            &thread,
            &target_bot_id,
            &message_id,
            &prompt,
        )
    } else {
        conversation(ctx.store.as_ref(), &thread, &message_id, &prompt)
    };
    let system_prompt = bots::build_system_prompt(ctx.store.as_ref(), &target_bot_id, room.is_group);
    let (model_override, backups_override) = bots::bot_model_override(&responding_bot);
    if let Err(error) = ctx.store.put("messages", &message_id, &user) {
        return Reply::error(500, error);
    }
    let turn = json!({
        "id": turn_id.clone(),
        "threadId": thread.clone(),
        "botId": target_bot_id.clone(),
        "status": "running",
        "source": "user",
        "createdAt": now.clone(),
        "finishedAt": Value::Null,
        "error": Value::Null
    });
    if let Err(error) = ctx.store.put("turns", &turn_id, &turn) {
        return Reply::error(500, error);
    }

    let active_model = model_override
        .as_ref()
        .and_then(|s| s.get("modelId").and_then(Value::as_str).map(String::from))
        .or_else(|| {
            ctx.store.setting("models").and_then(|v| {
                v.get("selection")
                    .and_then(|s| s.get("modelId"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
        });
    let started_at = iso_now();
    let initial_step = if room.is_group {
        "Preparing context & bot memory…".to_string()
    } else {
        "Preparing memory & conversation…".to_string()
    };
    set_thread_activity(
        ctx.live.as_ref(),
        &thread,
        vec![json!({
            "turnId": turn_id.clone(),
            "taskId": Value::Null,
            "title": target_bot_name.clone(),
            "step": initial_step.clone(),
            "thinking": Value::Null,
            "site": active_model.clone(),
            "since": started_at.clone(),
            "doing": "model",
            "quietSince": started_at.clone(),
            "model": active_model.clone(),
            "recent": []
        })],
    );
    ctx.refresh();

    let worker_store = Arc::clone(ctx.store);
    let worker_live = Arc::clone(ctx.live);
    let thread_for_worker = thread.clone();
    let worker_turn = turn_id.clone();
    let worker_bot_id = target_bot_id.clone();
    let worker_bot_name = target_bot_name.clone();
    let worker_bot_role = target_bot_role.clone();
    thread::spawn(move || {
        struct ProcessTracker<'a> {
            live: &'a Live,
            thread_id: &'a str,
            turn_id: &'a str,
            bot_name: &'a str,
            started: &'a str,
            model: &'a Option<String>,
            completed: Vec<Value>,
            current_step: String,
            latest_thinking: String,
            last_think_emit: Instant,
        }

        impl<'a> ProcessTracker<'a> {
            fn push_activity(&self) {
                set_thread_activity(
                    self.live,
                    self.thread_id,
                    vec![json!({
                        "turnId": self.turn_id,
                        "taskId": Value::Null,
                        "title": self.bot_name,
                        "step": self.current_step,
                        "thinking": if self.latest_thinking.is_empty() {
                            Value::Null
                        } else {
                            json!(tail_chars(&self.latest_thinking, 1200))
                        },
                        "site": self.model,
                        "since": self.started,
                        "doing": "model",
                        "quietSince": self.started,
                        "model": self.model,
                        "recent": self.completed
                    })],
                );
            }

            fn advance(&mut self, next: &str) {
                let next_trim = next.trim();
                if next_trim.is_empty() || next_trim == self.current_step {
                    return;
                }
                if !self.current_step.is_empty() {
                    let idx = self.completed.len() + 1;
                    self.completed.push(json!({
                        "id": format!("step-{idx}"),
                        "summary": self.current_step,
                        "site": Value::Null
                    }));
                }
                self.current_step = next_trim.to_string();
                self.push_activity();
            }

            fn update_thinking(&mut self, full_thinking: &str) {
                self.latest_thinking = full_thinking.to_string();
                if self.current_step != "Thinking…" {
                    self.advance("Thinking…");
                    self.last_think_emit = Instant::now();
                    return;
                }
                if self.last_think_emit.elapsed() >= StdDuration::from_millis(120) {
                    self.last_think_emit = Instant::now();
                    self.push_activity();
                }
            }
        }

        let stream_id = format!("turn:{worker_turn}");
        let mut tracker = ProcessTracker {
            live: worker_live.as_ref(),
            thread_id: &thread_for_worker,
            turn_id: &worker_turn,
            bot_name: &worker_bot_name,
            started: &started_at,
            model: &active_model,
            completed: Vec::new(),
            current_step: initial_step,
            latest_thinking: String::new(),
            last_think_emit: Instant::now() - StdDuration::from_secs(1),
        };
        let active_provider_label = model_override
            .as_ref()
            .and_then(|s| s.get("provider").and_then(Value::as_str).map(provider_name))
            .unwrap_or_default();
        if !active_provider_label.is_empty() {
            tracker.advance(&format!("Loaded context ({})", active_provider_label));
        } else {
            tracker.advance("Loaded context & memory");
        }

        #[derive(Default)]
        struct DirectiveStreamFilter {
            pending: String,
            hidden_close_tag: Option<&'static str>,
        }

        impl DirectiveStreamFilter {
            fn safe_prefix_len(haystack: &str) -> usize {
                for marker in ["<nod-page", "<nod-schedule"] {
                    let max = (marker.len() - 1).min(haystack.len());
                    for len in (1..=max).rev() {
                        if haystack.ends_with(&marker[..len]) {
                            let mut idx = haystack.len() - len;
                            while idx > 0 && !haystack.is_char_boundary(idx) {
                                idx -= 1;
                            }
                            return idx;
                        }
                    }
                }
                haystack.len()
            }

            fn push(&mut self, chunk: &str) -> (String, Option<&'static str>) {
                self.pending.push_str(chunk);
                let mut visible = String::new();
                let mut step_label: Option<&'static str> = None;

                loop {
                    if let Some(close) = self.hidden_close_tag {
                        if let Some(pos) = self.pending.find(close) {
                            let after = pos + close.len();
                            self.pending = self.pending[after..].to_string();
                            self.hidden_close_tag = None;
                            continue;
                        }
                        let keep = (close.len() - 1).min(self.pending.len());
                        let mut drop_end = self.pending.len().saturating_sub(keep);
                        while drop_end > 0 && !self.pending.is_char_boundary(drop_end) {
                            drop_end -= 1;
                        }
                        if drop_end > 0 {
                            self.pending = self.pending[drop_end..].to_string();
                        }
                        break;
                    }

                    let p_idx = self.pending.find("<nod-page");
                    let s_idx = self.pending.find("<nod-schedule");
                    let next = match (p_idx, s_idx) {
                        (Some(a), Some(b)) if a <= b => Some((a, "</nod-page>", "Creating page…")),
                        (Some(_), Some(b)) => Some((b, "</nod-schedule>", "Creating schedule…")),
                        (Some(a), None) => Some((a, "</nod-page>", "Creating page…")),
                        (None, Some(b)) => Some((b, "</nod-schedule>", "Creating schedule…")),
                        (None, None) => None,
                    };

                    if let Some((start, close, label)) = next {
                        visible.push_str(&self.pending[..start]);
                        self.pending = self.pending[start..].to_string();
                        self.hidden_close_tag = Some(close);
                        step_label = Some(label);
                        continue;
                    }

                    let safe_end = Self::safe_prefix_len(&self.pending);
                    if safe_end > 0 {
                        visible.push_str(&self.pending[..safe_end]);
                        self.pending = self.pending[safe_end..].to_string();
                    }
                    break;
                }

                (visible, step_label)
            }

            fn finish(&mut self) -> String {
                if self.hidden_close_tag.is_some() {
                    self.pending.clear();
                    return String::new();
                }
                std::mem::take(&mut self.pending)
            }
        }

        let mut directive_filter = DirectiveStreamFilter::default();
        let mut streamed_any = false;
        let result = match run_agent_with_tools(
            worker_store.as_ref(),
            worker_live.as_ref(),
            &worker_bot_id,
            false,
            None,
            &prompt,
            model_override.as_ref(),
            backups_override.as_deref(),
            Some(&system_prompt),
            &history,
            |evt| match evt {
                StreamEvent::Step(label) => tracker.advance(label),
                StreamEvent::Thinking(full_think) => tracker.update_thinking(full_think),
                StreamEvent::Delta(chunk) => {
                    let (visible, step_note) = directive_filter.push(chunk);
                    if let Some(step) = step_note {
                        tracker.advance(step);
                    }
                    if !visible.is_empty() {
                        streamed_any = true;
                        worker_live.broadcast(&json!({
                            "type": "delta",
                            "threadId": thread_for_worker,
                            "turnId": worker_turn,
                            "streamId": stream_id,
                            "botId": worker_bot_id,
                            "botName": worker_bot_name,
                            "botRole": worker_bot_role,
                            "text": visible,
                            "done": false
                        }));
                    }
                }
            },
        ) {
            Ok(agent_run) => {
                let outcome = agent_run.outcome;
                let tail = directive_filter.finish();
                if !tail.is_empty() {
                    streamed_any = true;
                    worker_live.broadcast(&json!({
                        "type": "delta",
                        "threadId": thread_for_worker,
                        "turnId": worker_turn,
                        "streamId": stream_id,
                        "botId": worker_bot_id,
                        "botName": worker_bot_name,
                        "botRole": worker_bot_role,
                        "text": tail,
                        "done": false
                    }));
                }
                if !streamed_any && !outcome.text.is_empty() {
                    let (visible, step_note) = directive_filter.push(&outcome.text);
                    if let Some(step) = step_note {
                        tracker.advance(step);
                    }
                    let tail2 = directive_filter.finish();
                    let full_vis = format!("{visible}{tail2}");
                    if !full_vis.is_empty() {
                        worker_live.broadcast(&json!({
                            "type": "delta",
                            "threadId": thread_for_worker,
                            "turnId": worker_turn,
                            "streamId": stream_id,
                            "botId": worker_bot_id,
                            "botName": worker_bot_name,
                            "botRole": worker_bot_role,
                            "text": full_vis,
                            "done": false
                        }));
                    }
                }
                if let Some(previous) = outcome.switched_from.as_deref() {
                    tracker.advance(&format!(
                        "Switched to backup model {} (from {previous})",
                        outcome.model_id
                    ));
                }
                super::usage::record_llm_usage(
                    worker_store.as_ref(),
                    worker_live.as_ref(),
                    &outcome.provider,
                    &outcome.model_id,
                    "chat",
                    &prompt,
                    outcome.input_tokens,
                    outcome.output_tokens,
                    outcome.cost_usd,
                    None,
                );
                if !outcome.thinking.is_empty() && tracker.latest_thinking.is_empty() {
                    tracker.latest_thinking = outcome.thinking.clone();
                }
                Ok((
                    outcome.text,
                    outcome.thinking,
                    agent_run.attachments,
                    agent_run.notes,
                ))
            }
            Err(error) if error == crate::llm::DEMO => {
                tracker.advance("Generating local demo response");
                thread::sleep(StdDuration::from_millis(350));
                Ok((
                    crate::llm::demo_reply(&prompt),
                    String::new(),
                    Vec::new(),
                    Vec::new(),
                ))
            }
            Err(error) => Err(error),
        };

        if !tracker.current_step.is_empty() && result.is_ok() {
            let idx = tracker.completed.len() + 1;
            tracker.completed.push(json!({
                "id": format!("step-{idx}"),
                "summary": tracker.current_step,
                "site": Value::Null
            }));
        }

        worker_live.broadcast(&json!({
            "type": "delta",
            "threadId": thread_for_worker,
            "turnId": worker_turn,
            "streamId": stream_id,
            "text": "",
            "done": true
        }));
        set_thread_activity(worker_live.as_ref(), &thread_for_worker, Vec::new());

        let processed_result = match result {
            Ok((raw_answer, thinking, mut tool_attachments, mut tool_notes)) => {
                let (mut clean_answer, directive_attachments, directive_notes) =
                    apply_workspace_directives(
                        worker_store.as_ref(),
                        worker_live.as_ref(),
                        &prompt,
                        &raw_answer,
                    );
                let mut seen_att_ids: std::collections::HashSet<String> = tool_attachments
                    .iter()
                    .filter_map(|a| a.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect();
                for att in directive_attachments {
                    let aid = text(&att, "id");
                    if aid.is_empty() || seen_att_ids.insert(aid) {
                        tool_attachments.push(att);
                    }
                }
                tool_notes.extend(directive_notes);
                for note in &tool_notes {
                    let idx = tracker.completed.len() + 1;
                    tracker.completed.push(json!({
                        "id": format!("step-{idx}"),
                        "summary": note,
                        "site": Value::Null
                    }));
                }
                if clean_answer.trim().is_empty() && !tool_attachments.is_empty() {
                    let names: Vec<String> = tool_attachments
                        .iter()
                        .map(|a| text(a, "name"))
                        .filter(|s| !s.is_empty())
                        .collect();
                    clean_answer = if names.is_empty() {
                        "Created page.".to_string()
                    } else {
                        format!("Saved {}", names.join(", "))
                    };
                }
                Ok((clean_answer, thinking, tool_attachments))
            }
            Err(err) => Err(err),
        };

        complete_turn_for_bot(
            &worker_store,
            TurnOutcomeTarget {
                thread: &thread_for_worker,
                turn_id: &worker_turn,
                bot_id: &worker_bot_id,
                bot_name: &worker_bot_name,
                bot_role: &worker_bot_role,
                stream_id: &stream_id,
            },
            &tracker.completed,
            processed_result,
        );
        worker_live.broadcast(&json!({ "type": "refresh" }));
    });

    Reply::ok(json!({
        "threadId": thread,
        "botId": target_bot_id,
        "turnId": turn_id,
        "messageId": message_id,
        "messageText": message_text,
        "attachments": []
    }))
}

fn artifact_view(artifact: &Value) -> Value {
    let id = text(artifact, "id");
    json!({
        "id": id,
        "name": text(artifact, "name"),
        "mimeType": text(artifact, "mimeType"),
        "size": artifact.get("size").and_then(Value::as_u64).unwrap_or(0),
        "url": artifact
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("/api/artifacts/{id}"))
    })
}

fn user_prompt(message: &str, attachments: &[Value]) -> String {
    if attachments.is_empty() {
        return message.to_string();
    }
    let names: Vec<String> = attachments.iter().map(|item| text(item, "name")).collect();
    if message.is_empty() {
        format!("The user attached: {}", names.join(", "))
    } else {
        format!("{message}\n\nAttached files: {}", names.join(", "))
    }
}

fn conversation(store: &Store, thread: &str, excluding: &str, prompt: &str) -> Vec<(String, String)> {
    let mut history: Vec<(String, String)> = messages(store, thread)
        .iter()
        .filter(|message| text(message, "id") != excluding)
        .filter_map(|message| {
            let role = text(message, "role");
            if role != "user" && role != "assistant" {
                return None;
            }
            if role == "assistant" && text(message, "streamId").starts_with("model-failure:") {
                return None;
            }
            let body = text(message, "text");
            if body.trim().is_empty() {
                return None;
            }
            Some((role, body))
        })
        .collect();
    history.push(("user".to_string(), prompt.to_string()));
    history
}

fn tail_chars(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        s.to_string()
    } else {
        s.chars().skip(total - max_chars).collect()
    }
}

pub struct AgentRunResult {
    pub outcome: crate::llm::CompletionOutcome,
    pub attachments: Vec<Value>,
    pub notes: Vec<String>,
}

fn sanitize_tool_ident(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').chars().take(64).collect()
}

fn is_readonly_mcp_tool(tool_name: &str) -> bool {
    let lower = tool_name.to_ascii_lowercase();
    let mutating_terms = [
        "create", "update", "delete", "remove", "post", "send", "submit", "add", "edit",
        "patch", "archive", "invite", "publish", "execute", "trigger", "write", "set_",
        "assign", "close", "reopen", "merge", "move",
    ];
    if mutating_terms.iter().any(|term| lower.contains(term)) {
        return false;
    }
    let read_prefixes = [
        "list", "get", "search", "find", "fetch", "read", "query", "view", "show", "check",
        "describe", "lookup", "inspect",
    ];
    read_prefixes
        .iter()
        .any(|prefix| lower.starts_with(prefix) || lower.contains(&format!("_{prefix}")))
}

fn owner_covers_schedule_create(owner_prompt: &str) -> bool {
    let lower = owner_prompt.to_ascii_lowercase();
    let cues = [
        "schedule",
        "every ",
        "daily",
        "weekly",
        "hourly",
        "monthly",
        "morning",
        "evening",
        "nightly",
        "cron",
        "remind",
        "routine",
        "recurring",
        "each day",
        "each week",
        "جدول",
        "كل يوم",
        "كل صباح",
        "تذكير",
    ];
    cues.iter().any(|c| lower.contains(c))
}

fn owner_covers_app_mutation(owner_prompt: &str, app_name: &str, service: &str, tool_name: &str) -> bool {
    let lower = owner_prompt.to_ascii_lowercase();
    if !app_name.is_empty() && lower.contains(&app_name.to_ascii_lowercase()) {
        return true;
    }
    if !service.is_empty() && lower.contains(&service.to_ascii_lowercase()) {
        return true;
    }
    for token in tool_name.split(['_', '-']).filter(|p| p.len() >= 4) {
        if lower.contains(&token.to_ascii_lowercase()) {
            return true;
        }
    }
    false
}

#[allow(clippy::type_complexity)]
pub fn build_turn_tools(
    store: &Store,
    include_schedule_create: bool,
) -> (Vec<crate::llm::ToolSpec>, std::collections::HashMap<String, (String, String)>) {
    let mut specs = vec![
        crate::llm::ToolSpec {
            name: "create_page".to_string(),
            description: "Save or update an interactive self-contained HTML Page and attach it to the conversation.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short page title" },
                    "filename": { "type": "string", "description": "Optional slug filename ending in .html" },
                    "html": { "type": "string", "description": "Complete self-contained HTML document" }
                },
                "required": ["title", "html"]
            }),
        },
        crate::llm::ToolSpec {
            name: "schedule_list".to_string(),
            description: "List the owner's configured schedules.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        },
        crate::llm::ToolSpec {
            name: "memory_find".to_string(),
            description: "Search accessible saved memory notes (read-only).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Keywords to match against memory notes" }
                },
                "required": ["query"]
            }),
        },
        crate::llm::ToolSpec {
            name: "web_fetch".to_string(),
            description: "Fetch a public HTTPS URL safely and return its readable text as untrusted data.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Public https:// URL to fetch" }
                },
                "required": ["url"]
            }),
        },
    ];

    if include_schedule_create {
        specs.push(crate::llm::ToolSpec {
            name: "schedule_create".to_string(),
            description: "Create a recurring or one-time schedule explicitly requested by the owner.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short schedule title" },
                    "prompt": { "type": "string", "description": "Instruction to execute on each scheduled run" },
                    "cron": { "type": "string", "description": "5-field cron expression, e.g. 0 8 * * *" }
                },
                "required": ["title", "prompt"]
            }),
        });
    }

    let mut alias_map = std::collections::HashMap::new();
    let has_connected_apps = store
        .all("apps")
        .into_iter()
        .any(|a| text(&a, "status") == "connected");
    if has_connected_apps {
        specs.push(crate::llm::ToolSpec {
            name: "SEARCH_TOOLS".to_string(),
            description: "Search tools and schemas across connected apps and MCP servers.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search terms or tool name" },
                    "connectionId": { "type": "string", "description": "Optional app connection ID filter" }
                },
                "required": ["query"]
            }),
        });
        specs.push(crate::llm::ToolSpec {
            name: "CONNECTED_MCP_CALL".to_string(),
            description: "Call one discovered MCP tool on a connected app account.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "connectionId": { "type": "string", "description": "Connected app account ID" },
                    "tool": { "type": "string", "description": "Exact MCP tool name" },
                    "input": { "type": "object", "description": "JSON arguments for the MCP tool" }
                },
                "required": ["connectionId", "tool"]
            }),
        });
        let mcp_tools = super::apps::connected_mcp_tools(store);
        for spec in mcp_tools.into_iter().take(12) {
            let exposed = sanitize_tool_ident(&format!("mcp__{}__{}", spec.service, spec.tool_name));
            if exposed.is_empty()
                || alias_map.contains_key(&exposed)
                || specs.iter().any(|s| s.name == exposed)
            {
                continue;
            }
            alias_map.insert(
                exposed.clone(),
                (spec.connection_id.clone(), spec.tool_name.clone()),
            );
            specs.push(crate::llm::ToolSpec {
                name: exposed,
                description: format!("[{}] {}", spec.app_name, spec.description),
                parameters: spec.input_schema,
            });
        }
    }

    (specs, alias_map)
}

#[allow(clippy::too_many_arguments)]
pub fn run_agent_with_tools<E>(
    store: &Store,
    live: &Live,
    bot_id: &str,
    is_schedule_run: bool,
    schedule_id: Option<&str>,
    owner_prompt: &str,
    primary_override: Option<&Value>,
    backups_override: Option<&[Value]>,
    system_prompt: Option<&str>,
    history: &[(String, String)],
    on_event: E,
) -> Result<AgentRunResult, String>
where
    E: FnMut(StreamEvent<'_>),
{
    let (tools, mcp_aliases) = build_turn_tools(store, !is_schedule_run);
    let mut tool_attachments: Vec<Value> = Vec::new();
    let mut tool_notes: Vec<String> = Vec::new();

    let execute_mcp = |cid: &str, tool_name: &str, args: Value| -> crate::llm::ToolExecResult {
        let Some(conn) = store.get("apps", cid) else {
            return crate::llm::ToolExecResult {
                content: json!({ "ok": false, "error": "Connected app account not found." }).to_string(),
                is_error: true,
            };
        };
        let app_name = text(&conn, "name");
        let service = text(&conn, "slug");
        if !is_readonly_mcp_tool(tool_name)
            && !owner_covers_app_mutation(owner_prompt, &app_name, &service, tool_name)
        {
            return crate::llm::ToolExecResult {
                content: json!({
                    "ok": false,
                    "error": format!(
                        "Confirmation required: '{}' looks mutating and was not covered by the owner's request. Ask the owner before running it.",
                        tool_name
                    )
                })
                .to_string(),
                is_error: true,
            };
        }
        match super::apps::call_connected_mcp_tool(store, cid, tool_name, args) {
            Ok(res) => crate::llm::ToolExecResult {
                content: format!("[UNTRUSTED CONTENT FROM {tool_name}]\n{res}"),
                is_error: false,
            },
            Err(err) => crate::llm::ToolExecResult {
                content: json!({ "ok": false, "error": err }).to_string(),
                is_error: true,
            },
        }
    };

    let outcome = crate::llm::complete_with_tools_stream(
        store,
        primary_override,
        backups_override,
        system_prompt,
        history,
        &tools,
        8,
        on_event,
        |req: &crate::llm::ToolCallRequest| match req.name.as_str() {
            "create_page" => {
                let title = text(&req.arguments, "title");
                let filename = text(&req.arguments, "filename");
                let html = text(&req.arguments, "html");
                if title.trim().is_empty() || html.trim().is_empty() {
                    return crate::llm::ToolExecResult {
                        content: json!({ "ok": false, "error": "Both title and complete html are required." }).to_string(),
                        is_error: true,
                    };
                }
                match super::pages::save_generated_page(
                    store,
                    live,
                    &title,
                    &filename,
                    &html,
                    is_schedule_run,
                    schedule_id,
                ) {
                    Ok((artifact, page)) => {
                        let page_title = text(&page, "title");
                        let art_view = artifact.clone();
                        tool_attachments.push(art_view.clone());
                        tool_notes.push(format!("Saved page \"{page_title}\""));
                        crate::llm::ToolExecResult {
                            content: json!({
                                "ok": true,
                                "pageId": text(&page, "id"),
                                "title": page_title,
                                "artifactId": art_view["id"],
                                "url": art_view["url"],
                            })
                            .to_string(),
                            is_error: false,
                        }
                    }
                    Err(err) => crate::llm::ToolExecResult {
                        content: json!({ "ok": false, "error": err }).to_string(),
                        is_error: true,
                    },
                }
            }
            "schedule_list" => {
                let rows: Vec<Value> = store
                    .all("schedules")
                    .into_iter()
                    .map(|s| {
                        json!({
                            "id": text(&s, "id"),
                            "title": text(&s, "title"),
                            "kind": text(&s, "kind"),
                            "cron": s.get("cron").cloned().unwrap_or(Value::Null),
                            "runAt": s.get("runAt").cloned().unwrap_or(Value::Null),
                            "enabled": s.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                            "nextRun": s.get("nextRun").cloned().unwrap_or(Value::Null),
                        })
                    })
                    .collect();
                crate::llm::ToolExecResult {
                    content: json!({ "schedules": rows }).to_string(),
                    is_error: false,
                }
            }
            "schedule_create" => {
                if is_schedule_run {
                    return crate::llm::ToolExecResult {
                        content: json!({
                            "ok": false,
                            "error": "A scheduled run cannot create schedules. Do the work this schedule asks for in this run instead."
                        })
                        .to_string(),
                        is_error: true,
                    };
                }
                if !owner_covers_schedule_create(owner_prompt) {
                    return crate::llm::ToolExecResult {
                        content: json!({
                            "ok": false,
                            "error": "The owner's message did not ask to create a schedule. Ask for confirmation first."
                        })
                        .to_string(),
                        is_error: true,
                    };
                }
                let title = text(&req.arguments, "title");
                let prompt = text(&req.arguments, "prompt");
                let cron = {
                    let c = text(&req.arguments, "cron");
                    if c.is_empty() { "0 8 * * *".to_string() } else { c }
                };
                match super::schedules::create_from_chat(store, live, &title, &cron, &prompt) {
                    Some(created) => {
                        tool_notes.push(format!("Scheduled \"{}\"", text(&created, "title")));
                        crate::llm::ToolExecResult {
                            content: json!({ "ok": true, "schedule": created }).to_string(),
                            is_error: false,
                        }
                    }
                    None => crate::llm::ToolExecResult {
                        content: json!({ "ok": false, "error": "Provide a non-empty schedule title and prompt." }).to_string(),
                        is_error: true,
                    },
                }
            }
            "memory_find" => {
                let q = text(&req.arguments, "query");
                let notes = bots::search_accessible_memories(store, bot_id, &q, 25);
                crate::llm::ToolExecResult {
                    content: json!({ "notes": notes }).to_string(),
                    is_error: false,
                }
            }
            "web_fetch" => {
                let raw_url = text(&req.arguments, "url");
                match super::apps::fetch_public_url_text(&raw_url, 12_000) {
                    Ok((final_url, body)) => crate::llm::ToolExecResult {
                        content: format!("[UNTRUSTED WEB CONTENT from {final_url}]\n{body}"),
                        is_error: false,
                    },
                    Err(err) => crate::llm::ToolExecResult {
                        content: json!({ "ok": false, "error": err }).to_string(),
                        is_error: true,
                    },
                }
            }
            "SEARCH_TOOLS" => {
                let q = text(&req.arguments, "query");
                let cid = text(&req.arguments, "connectionId");
                let filter = (!cid.is_empty()).then_some(cid.as_str());
                let res = super::apps::search_connected_tools(store, &q, filter);
                crate::llm::ToolExecResult {
                    content: format!("[UNTRUSTED CONTENT FROM SEARCH_TOOLS]\n{res}"),
                    is_error: false,
                }
            }
            "CONNECTED_MCP_CALL" => {
                let cid = text(&req.arguments, "connectionId");
                let tool = text(&req.arguments, "tool");
                let input = req.arguments.get("input").cloned().unwrap_or_else(|| json!({}));
                execute_mcp(&cid, &tool, input)
            }
            other => {
                if let Some((cid, real_tool)) = mcp_aliases.get(other) {
                    execute_mcp(cid, real_tool, req.arguments.clone())
                } else {
                    crate::llm::ToolExecResult {
                        content: json!({ "ok": false, "error": format!("Unknown tool '{other}'") }).to_string(),
                        is_error: true,
                    }
                }
            }
        },
    )?;

    Ok(AgentRunResult {
        outcome,
        attachments: tool_attachments,
        notes: tool_notes,
    })
}

struct TurnOutcomeTarget<'a> {
    thread: &'a str,
    turn_id: &'a str,
    bot_id: &'a str,
    bot_name: &'a str,
    bot_role: &'a str,
    stream_id: &'a str,
}

fn apply_workspace_directives(
    store: &Store,
    live: &Live,
    _user_prompt: &str,
    raw: &str,
) -> (String, Vec<Value>, Vec<String>) {
    let (without_pages, mut attachments, saved_pages) =
        super::pages::extract_and_save_pages(store, live, raw, false, None);
    let mut notes: Vec<String> = saved_pages
        .iter()
        .map(|p| format!("Saved page \"{}\"", text(p, "title")))
        .collect();

    let mut text_out = String::with_capacity(without_pages.len());
    let mut cursor = 0;
    while cursor < without_pages.len() {
        let rest = &without_pages[cursor..];
        let Some(rel_start) = rest.find("<nod-schedule") else {
            text_out.push_str(rest);
            break;
        };
        text_out.push_str(&rest[..rel_start]);
        let tag_slice = &rest[rel_start..];
        let Some(gt_pos) = tag_slice.find('>') else {
            text_out.push_str(tag_slice);
            break;
        };
        let attrs = &tag_slice[..gt_pos];
        let after_gt = &tag_slice[gt_pos + 1..];
        let Some(close_pos) = after_gt.find("</nod-schedule>") else {
            text_out.push_str(tag_slice);
            break;
        };
        let inner = &after_gt[..close_pos];
        cursor += rel_start + gt_pos + 1 + close_pos + "</nod-schedule>".len();

        let title = super::pages::parse_tag_attr(attrs, "title");
        let cron = super::pages::parse_tag_attr(attrs, "cron");
        let sched_prompt = inner.trim();
        if let Some(saved) =
            super::schedules::create_from_chat(store, live, &title, &cron, sched_prompt)
        {
            notes.push(format!("Created schedule \"{}\"", text(&saved, "title")));
        }
    }

    if attachments.is_empty() {
        let lower = text_out.to_ascii_lowercase();
        if let Some(open) = lower.find("```html").or_else(|| lower.find("```\n<!doctype")) {
            let line_end = text_out[open..].find('\n').map(|o| open + o + 1).unwrap_or(text_out.len());
            let after_open = &text_out[line_end..];
            if let Some(end_fence) = after_open.find("\n```") {
                let candidate_html = after_open[..end_fence].trim();
                let cand_lower = candidate_html.to_ascii_lowercase();
                if (cand_lower.contains("<!doctype html") || cand_lower.contains("<html"))
                    && cand_lower.contains("</html>")
                    && let Ok((artifact, page)) = super::pages::save_generated_page(
                        store,
                        live,
                        "",
                        "page.html",
                        candidate_html,
                        false,
                        None,
                    )
                {
                    let page_title = text(&page, "title");
                    attachments.push(artifact);
                    notes.push(format!("Saved page \"{page_title}\""));
                    let before = text_out[..open].trim_end();
                    let after = after_open[end_fence + 4..].trim_start();
                    text_out = match (before.is_empty(), after.is_empty()) {
                        (false, false) => format!("{before}\n\n{after}"),
                        (false, true) => before.to_string(),
                        (true, false) => after.to_string(),
                        (true, true) => String::new(),
                    };
                }
            }
        }
    }

    let mut clean = text_out.trim().to_string();
    if clean.is_empty() && !notes.is_empty() {
        clean = format!("Done: {}.", notes.join(" and "));
    }
    (clean, attachments, notes)
}

fn complete_turn_for_bot(
    store: &Store,
    target: TurnOutcomeTarget<'_>,
    process_steps: &[Value],
    result: Result<(String, String, Vec<Value>), String>,
) {
    if store.exists("stopped-turns", target.turn_id) {
        finish_turn(store, target.turn_id, "cancelled", Some("Stopped."));
        return;
    }
    match result {
        Ok((answer, thinking, attachments)) => {
            let assistant_id = uuid();
            let message = json!({
                "id": assistant_id,
                "threadId": target.thread,
                "botId": target.bot_id,
                "botName": target.bot_name,
                "botRole": target.bot_role,
                "role": "assistant",
                "text": answer,
                "thinkingText": if thinking.is_empty() { Value::Null } else { json!(thinking) },
                "processSteps": process_steps,
                "streamId": target.stream_id,
                "turnId": target.turn_id,
                "createdAt": iso_now(),
                "attachments": attachments,
                "reaction": Value::Null
            });
            if let Err(error) = store.put("messages", &assistant_id, &message) {
                finish_turn(store, target.turn_id, "failed", Some(&error));
                return;
            }
            finish_turn(store, target.turn_id, "succeeded", None);
        }
        Err(error) => {
            let assistant_id = uuid();
            let message = json!({
                "id": assistant_id,
                "threadId": target.thread,
                "botId": target.bot_id,
                "botName": target.bot_name,
                "botRole": target.bot_role,
                "role": "assistant",
                "text": error,
                "processSteps": process_steps,
                "turnId": target.turn_id,
                "createdAt": iso_now(),
                "attachments": [],
                "reaction": Value::Null,
                "streamId": format!("model-failure:{}", target.turn_id)
            });
            let _ = store.put("messages", &assistant_id, &message);
            finish_turn(store, target.turn_id, "failed", Some(&error));
        }
    }
}

fn finish_turn(store: &Store, turn_id: &str, status: &str, error: Option<&str>) {
    let mut turn = store.get("turns", turn_id).unwrap_or_else(|| {
        json!({
            "id": turn_id,
            "threadId": current_thread(store),
            "status": status,
            "source": "user",
            "createdAt": iso_now(),
            "finishedAt": Value::Null,
            "error": Value::Null
        })
    });
    if let Some(object) = turn.as_object_mut() {
        object.insert("status".to_string(), json!(status));
        object.insert("finishedAt".to_string(), json!(iso_now()));
        object.insert(
            "error".to_string(),
            error.map(|value| json!(value)).unwrap_or(Value::Null),
        );
    }
    if let Err(value) = store.put("turns", turn_id, &turn) {
        eprintln!("[Turn] Could not save turn {turn_id}: {value}");
    }
}

fn stop_thread(ctx: &Ctx) -> Reply {
    let thread = room_for_ctx(ctx, false).thread_id;
    let mut cancelled = 0;
    for mut turn in ctx.store.all("turns") {
        if turn.get("threadId").and_then(Value::as_str) != Some(thread.as_str()) {
            continue;
        }
        let status = turn.get("status").and_then(Value::as_str).unwrap_or_default();
        if status != "queued" && status != "running" {
            continue;
        }
        let Some(id) = turn.get("id").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        let _ = ctx.store.put("stopped-turns", &id, &json!({ "at": iso_now() }));
        if let Some(object) = turn.as_object_mut() {
            object.insert("status".to_string(), json!("cancelled"));
            object.insert("finishedAt".to_string(), json!(iso_now()));
            object.insert("error".to_string(), json!("Stopped."));
        }
        if ctx.store.put("turns", &id, &turn).is_ok() {
            cancelled += 1;
        }
    }
    set_thread_activity(ctx.live.as_ref(), &thread, Vec::new());
    ctx.refresh();
    Reply::ok(json!({ "ok": true, "cancelled": cancelled }))
}

fn reset(ctx: &Ctx) -> Reply {
    let room = room_for_ctx(ctx, false);
    let thread = new_thread(ctx.store);
    if room.is_group {
        bots::set_group_thread(ctx.store, &thread);
    } else {
        bots::set_bot_thread(ctx.store, &room.bot_id, &thread);
    }
    ctx.refresh();
    Reply::ok(json!({ "ok": true, "threadId": thread }))
}

fn restart(ctx: &Ctx) -> Reply {
    let thread = room_for_ctx(ctx, false).thread_id;
    let requested = super::text(&ctx.body, "threadId");
    if !requested.is_empty() && requested != thread {
        return Reply::error(
            409,
            "This conversation was reset. Reload it before restarting the chat.",
        );
    }
    ctx.refresh();
    Reply::ok(json!({ "ok": true }))
}

fn reaction(ctx: &Ctx, id: &str) -> Reply {
    let Some(mut message) = ctx.store.get("messages", id) else {
        return Reply::error(404, "Message not found.");
    };
    let value = ctx.body.get("reaction").cloned().unwrap_or(Value::Null);
    if !value.is_null() {
        let Some(reaction) = value.as_str() else {
            return Reply::error(400, "Choose a valid reaction.");
        };
        if reaction.chars().count() > 64 {
            return Reply::error(400, "That reaction is too long.");
        }
    }
    if let Some(object) = message.as_object_mut() {
        object.insert("reaction".to_string(), value);
    }
    if let Err(error) = ctx.store.put("messages", id, &message) {
        return Reply::error(500, error);
    }
    ctx.refresh();
    Reply::ok(message_view(&message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_activity_persists_across_thread_state_reads_until_cleared() {
        let live = Live::default();
        let tid = format!("thread-{}", uuid());
        assert!(thread_activity(&tid).is_empty());
        set_thread_activity(&live, &tid, vec![json!({ "turnId": "t1", "step": "Thinking…" })]);
        assert_eq!(thread_activity(&tid).len(), 1);
        assert_eq!(thread_activity(&tid)[0]["step"], "Thinking…");
        set_thread_activity(&live, &tid, Vec::new());
        assert!(thread_activity(&tid).is_empty());
    }

    #[test]
    fn workspace_directives_save_pages_and_schedules() {
        let store = Store::open_in_memory().unwrap();
        let live = Live::default();
        let raw = concat!(
            "Here is your reading tracker.\n",
            "<nod-page title=\"Reading List\" filename=\"reading-list.html\">\n",
            "<!doctype html><html><body><h1>Reading List</h1></body></html>\n",
            "</nod-page>\n",
            "<nod-schedule title=\"Refresh reading list\" cron=\"0 8 * * *\">\n",
            "Update the Reading List page with today's picks.\n",
            "</nod-schedule>"
        );
        let (clean, attachments, notes) = apply_workspace_directives(
            &store,
            &live,
            "make a page that tracks my reading list and update it every morning",
            raw,
        );
        assert_eq!(clean, "Here is your reading tracker.");
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0]["name"], "reading-list.html");
        assert_eq!(attachments[0]["mimeType"], "text/html");
        let aid = attachments[0]["id"].as_str().unwrap();
        assert_eq!(attachments[0]["url"], format!("/api/artifacts/{aid}"));
        assert_eq!(notes.len(), 2);
        assert_eq!(store.all("pages").len(), 1);
        assert_eq!(store.all("pages")[0]["title"], "Reading List");
        assert_eq!(store.all("pages")[0]["artifactId"], aid);
        assert_eq!(store.all("schedules").len(), 1);
        assert_eq!(store.all("schedules")[0]["title"], "Refresh reading list");
    }

    #[test]
    fn agent_tool_loop_creates_page_enforces_isolation_and_blocks_nested_schedules() {
        use tiny_http::{Header, Response, Server, StatusCode};

        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut req1 = server.recv().unwrap();
            let mut b1 = String::new();
            let _ = req1.as_reader().read_to_string(&mut b1);
            assert!(b1.contains("\"name\":\"memory_find\""));
            assert!(!b1.contains("\"name\":\"schedule_create\""));
            let sse1 = concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[",
                "{\"index\":0,\"id\":\"c_mem\",\"type\":\"function\",\"function\":{\"name\":\"memory_find\",\"arguments\":\"{\\\"query\\\":\\\"secret\\\"}\"}},",
                "{\"index\":1,\"id\":\"c_page\",\"type\":\"function\",\"function\":{\"name\":\"create_page\",\"arguments\":\"{\\\"title\\\":\\\"Status\\\",\\\"html\\\":\\\"<!doctype html><html><body>OK</body></html>\\\"}\"}}",
                "]}}]}\n\n",
                "data: [DONE]\n\n"
            );
            let mut r1 = Response::from_string(sse1).with_status_code(StatusCode(200));
            if let Ok(h) = Header::from_bytes("Content-Type", "text/event-stream") {
                r1.add_header(h);
            }
            let _ = req1.respond(r1);

            let mut req2 = server.recv().unwrap();
            let mut b2 = String::new();
            let _ = req2.as_reader().read_to_string(&mut b2);
            assert!(!b2.contains("TOP_SECRET_MAIN"), "secondary bot saw main memory: {b2}");
            assert!(b2.contains("SEC_ONLY_FACT"), "secondary bot missed own memory: {b2}");
            assert!(b2.contains("pageId"), "create_page output missing: {b2}");
            let sse2 = "data: {\"choices\":[{\"delta\":{\"content\":\"Page saved.\"}}]}\n\ndata: [DONE]\n\n";
            let mut r2 = Response::from_string(sse2).with_status_code(StatusCode(200));
            if let Ok(h) = Header::from_bytes("Content-Type", "text/event-stream") {
                r2.add_header(h);
            }
            let _ = req2.respond(r2);
        });

        let store = Store::open_in_memory().unwrap();
        let live = Live::default();
        store.set_setting(
            "models",
            &json!({
                "connections": [{
                    "id": "conn-a",
                    "provider": "compatible",
                    "method": "api_key",
                    "label": "Mock Agent",
                    "status": "connected",
                    "url": format!("http://127.0.0.1:{port}/v1"),
                    "models": [{ "id": "agent-m", "name": "Agent Model" }],
                    "key": "sk-a"
                }],
                "selection": { "connectionId": "conn-a", "modelId": "agent-m", "effort": "high" },
                "routes": {}
            }),
        );
        let sec_id = "bot-sec-1";
        let _ = store.put(
            "bots",
            sec_id,
            &json!({ "id": sec_id, "name": "Helper", "handle": "helper", "role": "secondary", "threadId": "t-sec" }),
        );
        let _ = store.put(
            "memories",
            "m-main",
            &json!({ "id": "m-main", "botId": bots::MAIN_BOT_ID, "area": "about", "path": "about/s1", "text": "TOP_SECRET_MAIN secret" }),
        );
        let _ = store.put(
            "memories",
            "m-sec",
            &json!({ "id": "m-sec", "botId": sec_id, "area": "about", "path": "about/s2", "text": "SEC_ONLY_FACT secret" }),
        );

        let res = run_agent_with_tools(
            &store,
            &live,
            sec_id,
            true,
            Some("sched-1"),
            "search secret and build status page",
            None,
            None,
            Some("Use tools."),
            &[("user".to_string(), "search secret".to_string())],
            |_| {},
        )
        .unwrap();

        assert_eq!(res.outcome.text, "Page saved.");
        assert_eq!(res.attachments.len(), 1);
        assert_eq!(res.attachments[0]["mimeType"], json!("text/html"));
        assert_eq!(store.all("pages").len(), 1);
        assert_eq!(store.all("pages")[0]["source"]["kind"], json!("schedule"));
        assert_eq!(store.all("pages")[0]["source"]["id"], json!("sched-1"));
        handle.join().unwrap();
    }
}
