use super::{text, Ctx, Reply};
use crate::db::Store;
use crate::util::{iso_now, uuid};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
const KEPT_VERSIONS: usize = 10;
const MAX_TITLE_CHARS: usize = 120;

/// Handles all `/api/pages` routes for saved interactive HTML pages.
pub fn handle(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    if path == "/api/pages" {
        return match method {
            "GET" => Some(list_pages(ctx)),
            "POST" => Some(create_page(ctx)),
            _ => None,
        };
    }

    let rest = path.strip_prefix("/api/pages/")?;
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match (method, parts.as_slice()) {
        ("GET", [id]) => Some(get_page(ctx, id)),
        ("PATCH", [id]) => Some(rename_page(ctx, id)),
        ("DELETE", [id]) => Some(delete_page(ctx, id)),
        ("GET", [id, "versions"]) => Some(list_versions(ctx, id)),
        ("POST", [id, "versions", rev, "restore"]) => Some(restore_version(ctx, id, rev)),
        ("POST", [id, "archive"]) => Some(set_archived(ctx, id, true)),
        ("POST", [id, "unarchive"]) => Some(set_archived(ctx, id, false)),
        _ => None,
    }
}

fn normalize_title(raw: &str) -> String {
    let collapsed: String = raw
        .chars()
        .map(|c| if c.is_control() || c.is_whitespace() { ' ' } else { c })
        .collect();
    collapsed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

fn derive_title_from_html(html: &str, fallback_name: &str) -> String {
    let lower = html.to_ascii_lowercase();
    if let Some(start) = lower.find("<title")
        && let Some(tag_end) = lower[start..].find('>')
    {
        let content_start = start + tag_end + 1;
        if let Some(end) = lower[content_start..].find("</title>") {
            let candidate = normalize_title(&html[content_start..content_start + end]);
            if !candidate.is_empty() {
                return candidate;
            }
        }
    }
    let stem = fallback_name
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(fallback_name);
    let candidate = normalize_title(stem);
    if candidate.is_empty() {
        "Untitled page".to_string()
    } else {
        candidate
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn file_view(store: &Store, artifact_id: &str) -> Value {
    let Some(artifact) = store.get("artifacts", artifact_id) else {
        return Value::Null;
    };
    let size = artifact.get("size").and_then(Value::as_u64).unwrap_or(0);
    let url = artifact
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("/api/artifacts/{artifact_id}"));
    json!({
        "id": artifact_id,
        "url": url,
        "size": size
    })
}

fn page_view(store: &Store, row: &Value) -> Value {
    let artifact_id = text(row, "artifactId");
    json!({
        "id": text(row, "id"),
        "title": text(row, "title"),
        "revision": row.get("revision").and_then(Value::as_i64).unwrap_or(1),
        "file": if artifact_id.is_empty() { Value::Null } else { file_view(store, &artifact_id) },
        "source": row.get("source").cloned().unwrap_or_else(|| json!({ "kind": "owner" })),
        "createdAt": text(row, "createdAt"),
        "updatedAt": text(row, "updatedAt"),
        "archivedAt": row.get("archivedAt").cloned().unwrap_or(Value::Null),
    })
}

fn sort_pages(rows: &mut [Value]) {
    rows.sort_by(|a, b| {
        let a_arch = a.get("archivedAt").and_then(Value::as_str).is_some();
        let b_arch = b.get("archivedAt").and_then(Value::as_str).is_some();
        a_arch
            .cmp(&b_arch)
            .then_with(|| text(b, "updatedAt").cmp(&text(a, "updatedAt")))
            .then_with(|| text(a, "id").cmp(&text(b, "id")))
    });
}

fn notify_pages(ctx: &Ctx) {
    ctx.live.broadcast(&json!({ "type": "pages" }));
    ctx.refresh();
}

fn sanitize_html_filename(raw: &str, fallback_title: &str) -> String {
    let base = raw.trim().rsplit(['/', '\\']).next().unwrap_or("");
    let stem = if base.is_empty() { fallback_title } else { base };
    let clean: String = stem
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = clean.trim_matches(['-', '.']);
    let base_name = if trimmed.is_empty() { "page" } else { trimmed };
    if base_name.ends_with(".html") || base_name.ends_with(".htm") {
        base_name.to_string()
    } else {
        format!("{base_name}.html")
    }
}

/// Saves or updates an interactive HTML page created during a chat or schedule run.
pub fn save_generated_page(
    store: &Store,
    live: &crate::server::Live,
    raw_title: &str,
    raw_filename: &str,
    html: &str,
    from_schedule: bool,
    schedule_id: Option<&str>,
) -> Result<(Value, Value), String> {
    let bytes = html.as_bytes();
    if bytes.is_empty() {
        return Err("Page HTML cannot be empty.".to_string());
    }
    if bytes.len() > MAX_PAGE_BYTES {
        return Err("Pages cannot be larger than 2 MiB.".to_string());
    }

    let derived = derive_title_from_html(html, raw_filename);
    let title = {
        let norm = normalize_title(raw_title);
        if norm.is_empty() { derived } else { norm }
    };
    let filename = sanitize_html_filename(raw_filename, &title);
    let digest = sha256_hex(bytes);
    let source = if from_schedule {
        json!({
            "kind": "schedule",
            "id": schedule_id.unwrap_or("schedule"),
            "title": title
        })
    } else {
        json!({ "kind": "chat", "current": true })
    };
    let now = iso_now();

    if let Some(mut existing) = store.all("pages").into_iter().find(|p| {
        p.get("archivedAt").and_then(Value::as_str).is_none()
            && text(p, "title").eq_ignore_ascii_case(&title)
    }) {
        let page_id = text(&existing, "id");
        if text(&existing, "digest") == digest {
            let prev_id = text(&existing, "artifactId");
            if let Some(prev_artifact) = (!prev_id.is_empty())
                .then(|| store.get("artifacts", &prev_id))
                .flatten()
            {
                live.broadcast(&json!({ "type": "pages" }));
                return Ok((prev_artifact, page_view(store, &existing)));
            }
        }
        let artifact_id = store.put_blob(bytes)?;
        let artifact = json!({
            "id": artifact_id,
            "name": filename,
            "mimeType": "text/html",
            "size": bytes.len(),
            "url": format!("/api/artifacts/{artifact_id}")
        });
        store.put("artifacts", &artifact_id, &artifact)?;
        let next_rev = existing
            .get("revision")
            .and_then(Value::as_i64)
            .unwrap_or(1)
            + 1;
        if let Some(obj) = existing.as_object_mut() {
            obj.insert("artifactId".to_string(), json!(artifact_id));
            obj.insert("digest".to_string(), json!(digest));
            obj.insert("revision".to_string(), json!(next_rev));
            obj.insert("source".to_string(), source.clone());
            obj.insert("updatedAt".to_string(), json!(now));
        }
        store.put("pages", &page_id, &existing)?;
        let ver_key = format!("{page_id}:{next_rev}");
        let ver_doc = json!({
            "id": ver_key,
            "pageId": page_id,
            "revision": next_rev,
            "artifactId": artifact_id,
            "digest": digest,
            "source": source,
            "createdAt": now,
        });
        store.put("page_versions", &ver_key, &ver_doc)?;
        for old in page_versions_for(store, &page_id).into_iter().skip(KEPT_VERSIONS) {
            let key = text(&old, "id");
            if !key.is_empty() {
                let _ = store.delete("page_versions", &key);
            }
        }
        live.broadcast(&json!({ "type": "pages" }));
        return Ok((artifact, page_view(store, &existing)));
    }

    let artifact_id = store.put_blob(bytes)?;
    let artifact = json!({
        "id": artifact_id,
        "name": filename,
        "mimeType": "text/html",
        "size": bytes.len(),
        "url": format!("/api/artifacts/{artifact_id}")
    });
    store.put("artifacts", &artifact_id, &artifact)?;

    let id = uuid();
    let doc = json!({
        "id": id,
        "title": title,
        "artifactId": artifact_id,
        "digest": digest,
        "revision": 1,
        "source": source,
        "createdAt": now,
        "updatedAt": now,
        "archivedAt": Value::Null,
    });
    store.put("pages", &id, &doc)?;
    let ver_key = format!("{id}:1");
    let ver_doc = json!({
        "id": ver_key,
        "pageId": id,
        "revision": 1,
        "artifactId": artifact_id,
        "digest": digest,
        "source": source,
        "createdAt": now,
    });
    store.put("page_versions", &ver_key, &ver_doc)?;
    live.broadcast(&json!({ "type": "pages" }));
    Ok((artifact, page_view(store, &doc)))
}

/// Parses an attribute value inside a tag header string.
pub fn parse_tag_attr(attrs: &str, name: &str) -> String {
    for quote in ['"', '\''] {
        let needle = format!("{name}={quote}");
        if let Some(start) = attrs.find(&needle) {
            let rest = &attrs[start + needle.len()..];
            if let Some(end) = rest.find(quote) {
                return rest[..end].trim().to_string();
            }
        }
    }
    String::new()
}

fn unwrap_html_fence(body: &str) -> &str {
    let trimmed = body.trim();
    if let Some(after_open) = trimmed.strip_prefix("```") {
        let inner = after_open.split_once('\n').map(|(_, rest)| rest).unwrap_or(after_open);
        if let Some(stripped) = inner.trim_end().strip_suffix("```") {
            return stripped.trim();
        }
    }
    trimmed
}

/// Extracts `<nod-page ...>...</nod-page>` blocks (and saves/updates each page), returning
/// `(stripped_text, saved_artifacts, saved_pages)`.
pub fn extract_and_save_pages(
    store: &Store,
    live: &crate::server::Live,
    raw: &str,
    from_schedule: bool,
    schedule_id: Option<&str>,
) -> (String, Vec<Value>, Vec<Value>) {
    let mut text_out = String::with_capacity(raw.len());
    let mut artifacts = Vec::new();
    let mut pages = Vec::new();
    let mut cursor = 0;

    while cursor < raw.len() {
        let rest = &raw[cursor..];
        let Some(rel_start) = rest.find("<nod-page") else {
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
        let Some(close_pos) = after_gt.find("</nod-page>") else {
            text_out.push_str(tag_slice);
            break;
        };
        let inner = &after_gt[..close_pos];
        cursor += rel_start + gt_pos + 1 + close_pos + "</nod-page>".len();

        let title = parse_tag_attr(attrs, "title");
        let filename = parse_tag_attr(attrs, "filename");
        let html = unwrap_html_fence(inner);
        if let Ok((artifact, page)) =
            save_generated_page(store, live, &title, &filename, html, from_schedule, schedule_id)
        {
            artifacts.push(artifact);
            pages.push(page);
        }
    }

    (text_out, artifacts, pages)
}

fn list_pages(ctx: &Ctx) -> Reply {
    let mut rows = ctx.store.all("pages");
    sort_pages(&mut rows);
    let views: Vec<Value> = rows.iter().map(|row| page_view(ctx.store.as_ref(), row)).collect();
    Reply::ok(Value::Array(views))
}

fn get_page(ctx: &Ctx, id: &str) -> Reply {
    let Some(row) = ctx.store.get("pages", id) else {
        return Reply::error(404, "Page not found.");
    };
    Reply::ok(page_view(ctx.store.as_ref(), &row))
}

fn create_page(ctx: &Ctx) -> Reply {
    let artifact_id = text(&ctx.body, "artifactId");
    if artifact_id.is_empty() {
        return Reply::error(400, "Select an HTML artifact to save as a page.");
    }
    let Some(artifact) = ctx.store.get("artifacts", &artifact_id) else {
        return Reply::error(404, "HTML artifact not found.");
    };
    let Some(bytes) = ctx.store.get_blob(&artifact_id) else {
        return Reply::error(404, "HTML file content is no longer available.");
    };
    if bytes.is_empty() {
        return Reply::error(400, "An empty HTML file cannot be saved as a page.");
    }
    if bytes.len() > MAX_PAGE_BYTES {
        return Reply::error(400, "Pages cannot be larger than 2 MiB.");
    }
    let Ok(html) = std::str::from_utf8(&bytes) else {
        return Reply::error(400, "Page HTML must be valid UTF-8 text.");
    };
    let digest = sha256_hex(&bytes);
    if let Some(existing) = ctx.store.all("pages").into_iter().find(|p| {
        p.get("archivedAt").and_then(Value::as_str).is_none() && text(p, "digest") == digest
    }) {
        return Reply::error(
            409,
            format!("This HTML file is already kept in Pages as “{}”.", text(&existing, "title")),
        );
    }

    let raw_title = ctx
        .body
        .get("title")
        .and_then(Value::as_str)
        .map(normalize_title)
        .unwrap_or_default();
    let title = if raw_title.is_empty() {
        derive_title_from_html(html, &text(&artifact, "name"))
    } else {
        raw_title
    };
    let now = iso_now();
    let id = uuid();
    let source = json!({ "kind": "owner" });
    let doc = json!({
        "id": id,
        "title": title,
        "artifactId": artifact_id,
        "digest": digest,
        "revision": 1,
        "source": source,
        "createdAt": now,
        "updatedAt": now,
        "archivedAt": Value::Null,
    });
    if let Err(err) = ctx.store.put("pages", &id, &doc) {
        return Reply::error(500, err);
    }
    let ver_key = format!("{id}:1");
    let ver_doc = json!({
        "id": ver_key,
        "pageId": id,
        "revision": 1,
        "artifactId": artifact_id,
        "digest": digest,
        "source": { "kind": "owner" },
        "createdAt": now,
    });
    if let Err(err) = ctx.store.put("page_versions", &ver_key, &ver_doc) {
        return Reply::error(500, err);
    }
    notify_pages(ctx);
    Reply::ok(page_view(ctx.store.as_ref(), &doc))
}

fn page_versions_for(store: &Store, page_id: &str) -> Vec<Value> {
    let mut rows: Vec<Value> = store
        .all("page_versions")
        .into_iter()
        .filter(|v| text(v, "pageId") == page_id)
        .collect();
    rows.sort_by(|a, b| {
        let ra = a.get("revision").and_then(Value::as_i64).unwrap_or(0);
        let rb = b.get("revision").and_then(Value::as_i64).unwrap_or(0);
        rb.cmp(&ra)
    });
    rows
}

fn list_versions(ctx: &Ctx, id: &str) -> Reply {
    if ctx.store.get("pages", id).is_none() {
        return Reply::error(404, "Page not found.");
    }
    let list: Vec<Value> = page_versions_for(ctx.store.as_ref(), id)
        .into_iter()
        .map(|v| {
            let aid = text(&v, "artifactId");
            json!({
                "revision": v.get("revision").and_then(Value::as_i64).unwrap_or(1),
                "file": if aid.is_empty() { Value::Null } else { file_view(ctx.store.as_ref(), &aid) },
                "source": v.get("source").cloned().unwrap_or_else(|| json!({ "kind": "owner" })),
                "createdAt": text(&v, "createdAt"),
            })
        })
        .collect();
    Reply::ok(Value::Array(list))
}

fn restore_version(ctx: &Ctx, id: &str, rev_str: &str) -> Reply {
    let Ok(target_rev) = rev_str.parse::<i64>() else {
        return Reply::error(400, "Invalid version number.");
    };
    let Some(mut page) = ctx.store.get("pages", id) else {
        return Reply::error(404, "Page not found.");
    };
    let current_rev = page.get("revision").and_then(Value::as_i64).unwrap_or(1);
    let expected_rev = ctx
        .body
        .get("revision")
        .and_then(Value::as_i64)
        .unwrap_or(current_rev);
    if expected_rev != current_rev {
        return Reply::error(
            409,
            "This page changed while you were restoring a version. Reload and try again.",
        );
    }

    let versions = page_versions_for(ctx.store.as_ref(), id);
    let Some(target) = versions
        .iter()
        .find(|v| v.get("revision").and_then(Value::as_i64) == Some(target_rev))
    else {
        return Reply::error(404, "Version not found.");
    };

    let artifact_id = text(target, "artifactId");
    let digest = text(target, "digest");
    let next_rev = current_rev + 1;
    let now = iso_now();

    if let Some(obj) = page.as_object_mut() {
        obj.insert("artifactId".to_string(), json!(artifact_id));
        obj.insert("digest".to_string(), json!(digest));
        obj.insert("revision".to_string(), json!(next_rev));
        obj.insert("updatedAt".to_string(), json!(now));
        obj.insert("source".to_string(), json!({ "kind": "owner" }));
    }
    if let Err(err) = ctx.store.put("pages", id, &page) {
        return Reply::error(500, err);
    }
    let ver_key = format!("{id}:{next_rev}");
    let ver_doc = json!({
        "id": ver_key,
        "pageId": id,
        "revision": next_rev,
        "artifactId": artifact_id,
        "digest": digest,
        "source": { "kind": "owner" },
        "createdAt": now,
    });
    if let Err(err) = ctx.store.put("page_versions", &ver_key, &ver_doc) {
        return Reply::error(500, err);
    }

    let updated_versions = page_versions_for(ctx.store.as_ref(), id);
    for old in updated_versions.into_iter().skip(KEPT_VERSIONS) {
        let key = text(&old, "id");
        if !key.is_empty() {
            let _ = ctx.store.delete("page_versions", &key);
        }
    }

    notify_pages(ctx);
    Reply::ok(page_view(ctx.store.as_ref(), &page))
}

fn rename_page(ctx: &Ctx, id: &str) -> Reply {
    let title = normalize_title(&text(&ctx.body, "title"));
    if title.is_empty() {
        return Reply::error(400, "A page title needs at least one visible character.");
    }
    let current = text(&ctx.body, "current");
    let Some(mut page) = ctx.store.get("pages", id) else {
        return Reply::error(404, "Page not found.");
    };
    let existing_title = text(&page, "title");
    if !current.is_empty() && existing_title != current {
        return Reply::error(
            409,
            format!("This page was renamed to “{existing_title}” while you were renaming it. Rename it again."),
        );
    }
    if existing_title != title
        && let Some(obj) = page.as_object_mut()
    {
        obj.insert("title".to_string(), json!(title));
        obj.insert("updatedAt".to_string(), json!(iso_now()));
        if let Err(err) = ctx.store.put("pages", id, &page) {
            return Reply::error(500, err);
        }
        notify_pages(ctx);
    }
    Reply::ok(page_view(ctx.store.as_ref(), &page))
}

fn set_archived(ctx: &Ctx, id: &str, archived: bool) -> Reply {
    let Some(mut page) = ctx.store.get("pages", id) else {
        return Reply::error(404, "Page not found.");
    };
    if let Some(obj) = page.as_object_mut() {
        let next = if archived {
            obj.get("archivedAt")
                .and_then(Value::as_str)
                .map(|s| json!(s))
                .unwrap_or_else(|| json!(iso_now()))
        } else {
            Value::Null
        };
        obj.insert("archivedAt".to_string(), next);
    }
    if let Err(err) = ctx.store.put("pages", id, &page) {
        return Reply::error(500, err);
    }
    notify_pages(ctx);
    Reply::ok(page_view(ctx.store.as_ref(), &page))
}

fn delete_page(ctx: &Ctx, id: &str) -> Reply {
    if ctx.store.get("pages", id).is_none() {
        return Reply::error(404, "Page not found.");
    }
    for ver in page_versions_for(ctx.store.as_ref(), id) {
        let key = text(&ver, "id");
        if !key.is_empty() {
            let _ = ctx.store.delete("page_versions", &key);
        }
    }
    let _ = ctx.store.delete("pages", id);
    notify_pages(ctx);
    Reply::ok(json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeManager;
    use crate::server::Live;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn test_env() -> (Arc<Store>, Arc<Live>, Arc<RuntimeManager>) {
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let live = Arc::new(Live::default());
        let runtime = RuntimeManager::new(Arc::clone(&live));
        (store, live, runtime)
    }

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
    fn pages_crud_duplicate_and_versions_work() {
        let (store, live, runtime) = test_env();
        let html = b"<!doctype html><html><head><title>  Revenue   Report </title></head><body>Hi</body></html>";
        let aid = store.put_blob(html).expect("blob");
        store
            .put(
                "artifacts",
                &aid,
                &json!({
                    "id": aid,
                    "name": "report.html",
                    "mimeType": "text/html",
                    "size": html.len(),
                    "url": format!("/api/artifacts/{aid}")
                }),
            )
            .expect("artifact");

        let created = call(&store, &live, &runtime, "POST", "/api/pages", json!({ "artifactId": aid }));
        assert_eq!(created.status, 200);
        assert_eq!(created.body["title"], "Revenue Report");
        let page_id = created.body["id"].as_str().unwrap().to_string();

        // Duplicate digest rejected with 409
        let dup = call(&store, &live, &runtime, "POST", "/api/pages", json!({ "artifactId": aid }));
        assert_eq!(dup.status, 409);

        // Rename works, and stale `current` returns 409
        let renamed = call(
            &store,
            &live,
            &runtime,
            "PATCH",
            &format!("/api/pages/{page_id}"),
            json!({ "title": "Q4 Board", "current": "Revenue Report" }),
        );
        assert_eq!(renamed.status, 200);
        assert_eq!(renamed.body["title"], "Q4 Board");

        let conflict = call(
            &store,
            &live,
            &runtime,
            "PATCH",
            &format!("/api/pages/{page_id}"),
            json!({ "title": "Other", "current": "Revenue Report" }),
        );
        assert_eq!(conflict.status, 409);

        // Archive and restore version 1
        let archived = call(&store, &live, &runtime, "POST", &format!("/api/pages/{page_id}/archive"), json!({}));
        assert_eq!(archived.status, 200);
        assert!(archived.body["archivedAt"].as_str().is_some());

        let restored = call(
            &store,
            &live,
            &runtime,
            "POST",
            &format!("/api/pages/{page_id}/versions/1/restore"),
            json!({ "revision": 1 }),
        );
        assert_eq!(restored.status, 200);
        assert_eq!(restored.body["revision"], 2);

        let versions = call(&store, &live, &runtime, "GET", &format!("/api/pages/{page_id}/versions"), Value::Null);
        assert_eq!(versions.body.as_array().unwrap().len(), 2);

        let deleted = call(&store, &live, &runtime, "DELETE", &format!("/api/pages/{page_id}"), Value::Null);
        assert_eq!(deleted.status, 200);
        assert!(store.get("pages", &page_id).is_none());

        let (art1, pg1) = save_generated_page(&store, &live, "Dashboard", "dash.html", "<!doctype html><html><body>v1</body></html>", false, None).expect("save 1");
        let (art2, pg2) = save_generated_page(&store, &live, "Dashboard", "dash.html", "<!doctype html><html><body>v1</body></html>", false, None).expect("save 2");
        assert_eq!(art1["id"], art2["id"]);
        assert_eq!(pg1["id"], pg2["id"]);
        assert_eq!(pg2["file"]["id"], art2["id"]);
    }
}
