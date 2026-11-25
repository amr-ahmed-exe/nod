use super::{Ctx, Reply};
use serde_json::json;
use std::io::Write;

pub fn handle(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    match (method, path) {
        ("POST", "/api/log-error") => Some(log_error(ctx)),
        ("GET", "/api/onboarding/introduction") => Some(onboarding(ctx)),
        ("GET", "/api/onboarding/suggestions") => Some(Reply::ok(json!({ "suggestions": [] }))),
        ("POST", "/api/feedback") => Some(feedback(ctx)),
        ("GET", "/api/surveys") => Some(Reply::ok(json!({ "surveys": [] }))),
        ("POST", "/api/surveys") => Some(Reply::ok(json!({ "ok": true }))),
        ("GET", "/api/skills") => Some(Reply::ok(json!([]))),
        ("GET", "/api/prepared") => Some(Reply::ok(json!([]))),
        ("GET", "/api/access") => Some(Reply::ok(json!({ "rules": [], "enabled": true }))),
        ("POST", "/api/messages/audio") => Some(audio(ctx)),
        ("GET", "/api/artifacts") => Some(artifacts(ctx)),
        ("POST", "/api/uploads") => Some(uploads(ctx)),
        ("POST", "/api/uploads/renew") => Some(renew_upload(ctx)),
        ("GET", "/api/discovered") => Some(discovered(ctx)),
        ("POST", "/api/bug-report") => Some(bug_report(ctx)),
        _ => {
            if let Some(id) = path.strip_prefix("/api/artifacts/")
                && method == "GET"
            {
                return Some(artifact_blob(ctx, id));
            }
            if let Some(id) = path.strip_prefix("/api/prepared/")
                && method == "GET"
            {
                return Some(match ctx.store.get("prepared", id.trim_matches('/')) {
                    Some(item) => Reply::ok(item),
                    None => Reply::error(404, "Prepared work item not found."),
                });
            }
            None
        }
    }
}

fn log_error(ctx: &Ctx) -> Reply {
    let content = ctx.body.to_string();
    eprintln!("[FRONTEND ERROR] {content}");
    let log_path = ctx.store.document_path("frontend-error.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(file, "{content}");
    }
    Reply::ok(json!({ "ok": true }))
}

fn onboarding(_ctx: &Ctx) -> Reply {
    Reply::ok(json!({ "messageId": null }))
}

fn feedback(ctx: &Ctx) -> Reply {
    let id = crate::runtime::storage::random_hex(8);
    if let Err(err) = ctx.store.put(
        "feedback",
        &id,
        &json!({ "id": id, "owner": ctx.owner, "payload": ctx.body }),
    ) {
        return Reply::error(500, err);
    }
    Reply::ok(json!({ "ok": true }))
}

fn audio(_ctx: &Ctx) -> Reply {
    Reply::json(
        501,
        json!({
            "error": "Voice messages are not available in the native build.",
            "status": "setup_required"
        }),
    )
}

fn artifacts(ctx: &Ctx) -> Reply {
    let rows = ctx.store.all("artifacts");
    Reply::ok(serde_json::Value::Array(rows))
}

fn uploads(ctx: &Ctx) -> Reply {
    let (name, mime, bytes) = if let Some((file_name, file_mime, raw)) =
        parse_multipart_file(ctx.content_type, ctx.body_bytes)
    {
        (file_name, file_mime, raw)
    } else if !ctx.body_bytes.is_empty() {
        (
            format!("upload-{}.bin", crate::runtime::storage::random_hex(4)),
            if ctx.content_type.is_empty() {
                "application/octet-stream".to_string()
            } else {
                ctx.content_type.to_string()
            },
            ctx.body_bytes.to_vec(),
        )
    } else {
        return Reply::error(400, "Choose a file to upload.");
    };

    let blob_id = match ctx.store.put_blob(&bytes) {
        Ok(id) => id,
        Err(err) => return Reply::error(500, err),
    };
    let artifact = json!({
        "id": blob_id,
        "name": name,
        "mimeType": mime,
        "size": bytes.len(),
        "url": format!("/api/artifacts/{blob_id}")
    });
    if let Err(err) = ctx.store.put("artifacts", &blob_id, &artifact) {
        return Reply::error(500, err);
    }
    Reply::ok(artifact)
}

fn renew_upload(ctx: &Ctx) -> Reply {
    let id = super::text(&ctx.body, "id");
    match ctx.store.get("artifacts", &id) {
        Some(item) => Reply::ok(item),
        None => Reply::error(404, "Uploaded file not found."),
    }
}

fn artifact_blob(ctx: &Ctx, raw_id: &str) -> Reply {
    let id = raw_id.split('/').next().unwrap_or("");
    let Some(bytes) = ctx.store.get_blob(id) else {
        return Reply::error(404, "Artifact not found.");
    };
    let mime = ctx
        .store
        .get("artifacts", id)
        .and_then(|v| v.get("mimeType").and_then(serde_json::Value::as_str).map(str::to_string))
        .map(|m| {
            if m == "text/html" {
                "text/html; charset=utf-8".to_string()
            } else {
                m
            }
        })
        .unwrap_or_else(|| "application/octet-stream".to_string());
    Reply::bytes(200, bytes, mime)
}

fn parse_multipart_file(content_type: &str, body: &[u8]) -> Option<(String, String, Vec<u8>)> {
    let boundary = content_type
        .split(';')
        .find_map(|part| part.trim().strip_prefix("boundary="))?
        .trim_matches('"');
    let marker = format!("--{boundary}").into_bytes();
    let end_headers = b"\r\n\r\n";

    let mut pos = find_subslice(body, &marker)?;
    pos += marker.len();
    while pos < body.len() {
        if body[pos..].starts_with(b"--") {
            break;
        }
        if body[pos..].starts_with(b"\r\n") {
            pos += 2;
        }
        let header_end = find_subslice(&body[pos..], end_headers)?;
        let header_str = String::from_utf8_lossy(&body[pos..pos + header_end]);
        let content_start = pos + header_end + end_headers.len();
        let next_boundary = find_subslice(&body[content_start..], &marker)?;
        let mut content_end = content_start + next_boundary;
        if content_end >= 2 && &body[content_end - 2..content_end] == b"\r\n" {
            content_end -= 2;
        }
        if header_str.contains("filename=") {
            let name = header_str
                .split("filename=\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or("upload.bin")
                .to_string();
            let mime = header_str
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-type:"))
                .and_then(|l| l.split(':').nth(1))
                .map(str::trim)
                .unwrap_or("application/octet-stream")
                .to_string();
            return Some((name, mime, body[content_start..content_end].to_vec()));
        }
        pos = content_start + next_boundary + marker.len();
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn discovered(ctx: &Ctx) -> Reply {
    Reply::ok(json!({ "connections": [], "ready": ctx.runtime.is_ready() }))
}

fn bug_report(ctx: &Ctx) -> Reply {
    let id = crate::runtime::storage::random_hex(8);
    if let Err(err) = ctx.store.put(
        "bug_reports",
        &id,
        &json!({ "id": id, "owner": ctx.owner, "payload": ctx.body }),
    ) {
        return Reply::error(500, err);
    }
    Reply::ok(json!({
        "ok": true,
        "message": "Bug report saved locally. Thank you!"
    }))
}
