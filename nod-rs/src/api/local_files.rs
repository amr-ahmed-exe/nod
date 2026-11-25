//! Owner-scoped local files routes adhering strictly to user preferences and path boundaries.

use super::{preferences_state, Ctx, Reply};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const SECRET_NAMES: &[&str] = &[
    ".env",
    ".ssh",
    ".aws",
    ".gnupg",
    ".git-credentials",
    ".npmrc",
    "id_rsa",
    "id_ed25519",
    "ntuser.dat",
    "state.json",
];

/// Handles `/api/local-files*` routes safely under user preference opt-in and secret filtering.
pub fn handle(ctx: &Ctx) -> Option<Reply> {
    if !ctx.path.starts_with("/api/local-files") {
        return None;
    }
    let prefs = preferences_state(ctx.store);
    let enabled = prefs
        .get("localFilesEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if ctx.path == "/api/local-files/keep-out" {
        return Some(handle_keep_out(ctx, enabled));
    }
    if !enabled {
        return Some(Reply::json(
            403,
            json!({
                "error": "Local file access is turned off in Preferences.",
                "status": "disabled"
            }),
        ));
    }

    if !ctx.runtime.is_ready() {
        return Some(Reply::json(
            503,
            json!({
                "error": "Private computer runtime is not ready.",
                "status": "setup_required"
            }),
        ));
    }

    match (ctx.method, ctx.path) {
        ("GET", "/api/local-files") | ("GET", "/api/local-files/roots") => Some(list_dir(ctx)),
        _ => Some(Reply::json(
            501,
            json!({
                "error": "Local file modifications are not enabled in this build.",
                "status": "setup_required"
            }),
        )),
    }
}

fn allowed_roots() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("USERPROFILE").map(PathBuf::from) else {
        return Vec::new();
    };
    ["Desktop", "Documents", "Downloads"]
        .iter()
        .filter_map(|name| {
            let p = home.join(name);
            p.is_dir().then(|| p.canonicalize().unwrap_or(p))
        })
        .collect()
}

fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_NAMES.iter().any(|s| lower == *s || lower.starts_with(".env."))
        || lower.ends_with(".key")
        || lower.ends_with(".pem")
        || lower.ends_with(".pfx")
}

fn list_dir(ctx: &Ctx) -> Reply {
    let roots = allowed_roots();
    if let Some(requested) = ctx.query.get("path").map(String::as_str).filter(|p| !p.is_empty()) {
        let Ok(canon) = Path::new(requested).canonicalize() else {
            return Reply::error(404, "Folder not found.");
        };
        if !roots.iter().any(|root| canon.starts_with(root)) {
            return Reply::error(403, "Path is outside allowed user folders.");
        }
        if canon
            .file_name()
            .and_then(|n| n.to_str())
            .map(is_secret_name)
            .unwrap_or(false)
        {
            return Reply::error(403, "Access to sensitive files is blocked.");
        }
        let Ok(entries) = fs::read_dir(&canon) else {
            return Reply::error(500, "Could not read folder.");
        };
        let mut items = Vec::new();
        for entry in entries.flatten().take(500) {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_secret_name(&name) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            items.push(json!({
                "name": name,
                "path": entry.path().to_string_lossy(),
                "isDirectory": meta.is_dir(),
                "size": meta.len()
            }));
        }
        return Reply::ok(json!({ "path": canon.to_string_lossy(), "entries": items }));
    }

    let root_entries: Vec<Value> = roots
        .into_iter()
        .map(|p| {
            json!({
                "name": p.file_name().and_then(|n| n.to_str()).unwrap_or("Folder"),
                "path": p.to_string_lossy()
            })
        })
        .collect();
    Reply::ok(json!({ "roots": root_entries }))
}

fn kept_out_paths(store: &crate::db::Store, owner: &str) -> Vec<String> {
    store
        .setting(&format!("local_files_keep_out:{owner}"))
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn handle_keep_out(ctx: &Ctx, enabled: bool) -> Reply {
    let available = ctx.runtime.is_ready();
    let key = format!("local_files_keep_out:{}", ctx.owner);
    if ctx.method == "GET" {
        let paths = if available && enabled {
            kept_out_paths(ctx.store, ctx.owner)
        } else {
            Vec::new()
        };
        return Reply::ok(json!({ "available": available, "paths": paths }));
    }
    if ctx.method == "DELETE" {
        if !available {
            return Reply::json(
                503,
                json!({ "error": "Local file access is not available right now." }),
            );
        }
        let target = ctx
            .body
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if target.is_empty() {
            return Reply::error(400, "Name the folder to let Nod into.");
        }
        let mut paths = kept_out_paths(ctx.store, ctx.owner);
        paths.retain(|item| !item.eq_ignore_ascii_case(target));
        ctx.store.set_setting(&key, &json!(paths));
        ctx.refresh();
        return Reply::ok(json!({ "available": available, "paths": paths }));
    }
    Reply::json(
        501,
        json!({ "error": "Unsupported keep-out operation.", "status": "setup_required" }),
    )
}
