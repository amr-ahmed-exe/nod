//! Owner-scoped memory view, notes, and preferences matching `shared/memory.ts`.

use super::{bots, text, Ctx, Reply};
use crate::util::{iso_now, uuid};
use serde_json::{json, Value};

pub fn handle(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    match (method, path) {
        ("GET", "/api/memory") => Some(view_reply(ctx)),
        ("PATCH", "/api/memory") => Some(patch_settings(ctx)),
        ("GET", "/api/memory/export") => Some(export_markdown(ctx)),
        ("POST", "/api/memory") | ("POST", "/api/memory/notes") => Some(create(ctx)),
        ("POST", "/api/memory/import") => Some(import(ctx)),
        _ => path
            .strip_prefix("/api/memory/")
            .map(|rest| by_subpath(ctx, rest)),
    }
}

fn target_bot_id(ctx: &Ctx) -> String {
    let requested = ctx
        .param("botId")
        .or_else(|| ctx.body.get("botId").and_then(Value::as_str).map(String::from));
    bots::resolve_memory_bot_id(ctx.store, requested.as_deref())
}

fn memory_settings(ctx: &Ctx, bot_id: &str) -> (bool, bool) {
    bots::bot_memory_settings(ctx.store, bot_id)
}

fn to_memory_note(raw: &Value) -> Option<Value> {
    let id = raw
        .get("path")
        .or_else(|| raw.get("id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let area = raw
        .get("area")
        .and_then(Value::as_str)
        .filter(|a| matches!(*a, "about" | "preferences" | "people" | "routines" | "lessons"))
        .unwrap_or("about");
    let text_val = raw
        .get("text")
        .or_else(|| raw.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let status = raw
        .get("status")
        .and_then(Value::as_str)
        .filter(|s| matches!(*s, "stated" | "observed" | "tentative"))
        .unwrap_or("stated");
    let updated = raw
        .get("updated")
        .or_else(|| raw.get("updatedAt"))
        .or_else(|| raw.get("createdAt"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(iso_now);
    let path = if id.contains('/') {
        id.to_string()
    } else {
        format!("{area}/{id}")
    };
    let bot_id = raw
        .get("botId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(bots::MAIN_BOT_ID);
    Some(json!({
        "path": path,
        "area": area,
        "text": text_val,
        "status": status,
        "updated": updated,
        "botId": bot_id,
        "edited": raw.get("edited").cloned().unwrap_or(Value::Null),
        "expires": raw.get("expires").cloned().unwrap_or(Value::Null),
        "learnedFrom": raw.get("learnedFrom").cloned().unwrap_or(Value::Null),
        "sources": raw.get("sources").cloned().unwrap_or_else(|| json!([])),
    }))
}

fn build_view(ctx: &Ctx) -> Value {
    let bot_id = target_bot_id(ctx);
    let (saving, sensitive) = memory_settings(ctx, &bot_id);
    let all_memories = ctx.store.all("memories");
    let notes: Vec<Value> = all_memories
        .iter()
        .filter(|raw| bots::note_belongs_to_bot(raw, &bot_id))
        .filter_map(to_memory_note)
        .collect();

    // Only Main Bot receives read-only summaries of secondary bots' memories.
    let visible_bots_memories = if bot_id == bots::MAIN_BOT_ID {
        bots::context_snapshot(ctx.store, &bot_id)
            .and_then(|snap| snap.get("visibleBots").cloned())
            .unwrap_or_else(|| json!([]))
    } else {
        json!([])
    };

    json!({
        "botId": bot_id,
        "saving": saving,
        "sensitive": sensitive,
        "notes": notes,
        "visibleBotsMemories": visible_bots_memories,
        "archive": [],
        "skills": []
    })
}

fn view_reply(ctx: &Ctx) -> Reply {
    Reply::ok(build_view(ctx))
}

fn patch_settings(ctx: &Ctx) -> Reply {
    let bot_id = target_bot_id(ctx);
    bots::update_bot_memory_settings(ctx.store, &bot_id, &ctx.body);
    ctx.refresh();
    Reply::ok(build_view(ctx))
}

fn export_markdown(ctx: &Ctx) -> Reply {
    let view = build_view(ctx);
    let mut out = String::from("# Nod Memory\n\n");
    if let Some(notes) = view.get("notes").and_then(Value::as_array) {
        for note in notes {
            let area = note.get("area").and_then(Value::as_str).unwrap_or("about");
            let body = note.get("text").and_then(Value::as_str).unwrap_or("");
            if !body.is_empty() {
                out.push_str(&format!("- [{area}] {body}\n"));
            }
        }
    }
    Reply::text(200, out, "text/markdown; charset=utf-8")
}

fn create(ctx: &Ctx) -> Reply {
    let id = uuid();
    let content = {
        let primary = text(&ctx.body, "text");
        if primary.is_empty() {
            text(&ctx.body, "content")
        } else {
            primary
        }
    };
    if content.trim().is_empty() {
        return Reply::error(400, "Enter a memory.");
    }
    let raw_area = text(&ctx.body, "area");
    let area = match raw_area.as_str() {
        a @ ("about" | "preferences" | "people" | "routines" | "lessons") => a,
        _ => "about",
    };
    let bot_id = target_bot_id(ctx);
    let path = format!("{area}/{id}");
    let now = iso_now();
    let memory = json!({
        "id": id,
        "botId": bot_id,
        "path": path,
        "area": area,
        "text": content,
        "content": content,
        "status": "stated",
        "source": "manual",
        "updated": now,
        "createdAt": now,
        "updatedAt": now,
        "sources": []
    });
    let _ = ctx.store.put("memories", &id, &memory);
    ctx.refresh();
    let mut update = build_view(ctx);
    if let Some(obj) = update.as_object_mut() {
        obj.insert("path".to_string(), json!(path));
    }
    Reply::ok(update)
}

fn import(ctx: &Ctx) -> Reply {
    let bot_id = target_bot_id(ctx);
    let items = ctx
        .body
        .get("items")
        .or_else(|| ctx.body.get("notes"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut added = 0;
    let mut skipped = 0;
    for item in &items {
        let content = item
            .get("text")
            .or_else(|| item.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if content.trim().is_empty() {
            skipped += 1;
            continue;
        }
        let id = uuid();
        let now = iso_now();
        let memory = json!({
            "id": id,
            "botId": bot_id,
            "path": format!("about/{id}"),
            "area": "about",
            "text": content,
            "content": content,
            "status": "stated",
            "source": "import",
            "updated": now,
            "createdAt": now,
            "updatedAt": now,
            "sources": []
        });
        let _ = ctx.store.put("memories", &id, &memory);
        added += 1;
    }
    if added > 0 {
        ctx.refresh();
    }
    let mut update = build_view(ctx);
    if let Some(obj) = update.as_object_mut() {
        obj.insert(
            "imported".to_string(),
            json!({ "added": added, "existing": 0, "skipped": skipped }),
        );
    }
    Reply::ok(update)
}

fn by_subpath(ctx: &Ctx, rest: &str) -> Reply {
    let bot_id = target_bot_id(ctx);
    let trimmed = rest.trim_start_matches("notes/");
    let segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    let id = segments
        .last()
        .copied()
        .filter(|s| *s != "archive")
        .unwrap_or("");
    let actual_id = if rest.ends_with("/archive") && segments.len() >= 2 {
        segments[segments.len() - 2]
    } else {
        id
    };
    if let Some(existing) = ctx.store.get("memories", actual_id)
        && !bots::is_note_accessible_to_bot(&existing, &bot_id)
    {
        return Reply::error(404, "Memory not found.");
    }
    match ctx.method {
        "GET" => match ctx.store.get("memories", actual_id) {
            Some(memory) => Reply::ok(memory),
            None => Reply::error(404, "Memory not found."),
        },
        "PATCH" => match ctx.store.patch("memories", actual_id, &ctx.body) {
            Some(_) => {
                ctx.refresh();
                Reply::ok(build_view(ctx))
            }
            None => Reply::error(404, "Memory not found."),
        },
        "DELETE" | "POST" => {
            ctx.store.delete("memories", actual_id);
            ctx.refresh();
            Reply::ok(build_view(ctx))
        }
        _ => Reply::error(404, "Memory not found."),
    }
}
