use super::{text, Ctx, Reply};
use crate::db::Store;
use crate::util::{iso_now, uuid};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// Handles all `/api/jobs/*` endpoints (CVs, profile, templates, postings, companies, people, sources, helper).
pub fn handle(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    match (method, path) {
        ("GET", "/api/jobs/setup") => Some(setup(ctx)),
        ("GET", "/api/jobs/cvs") => Some(cvs(ctx)),
        ("POST", "/api/jobs/cvs") => Some(upload_cv(ctx)),
        ("GET", "/api/jobs/profile") => Some(profile(ctx)),
        ("PATCH", "/api/jobs/profile") | ("PUT", "/api/jobs/profile") => Some(update_profile(ctx)),
        ("GET", "/api/jobs/profile/revisions") => Some(profile_revisions(ctx)),
        ("GET", "/api/jobs/templates") => Some(templates(ctx)),
        ("PUT", "/api/jobs/templates") | ("PATCH", "/api/jobs/templates") => {
            Some(update_templates(ctx))
        }
        ("GET", "/api/jobs/companies") => Some(list_companies(ctx)),
        ("GET", "/api/jobs/postings") => Some(list_postings(ctx)),
        ("GET", "/api/jobs/sources") => Some(list_sources(ctx)),
        ("POST", "/api/jobs/sources/telegram") => Some(add_telegram_channel(ctx)),
        ("GET", "/api/jobs/helper") => Some(get_helper(ctx)),
        ("POST", "/api/jobs/helper/pairing") => Some(create_helper_pairing(ctx)),
        ("DELETE", "/api/jobs/helper/pairing") => Some(cancel_helper_pairing(ctx)),
        _ => dispatch_dynamic(ctx),
    }
}

fn dispatch_dynamic(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    if let Some(rev) = path
        .strip_prefix("/api/jobs/profile/revisions/")
        .and_then(|s| s.strip_suffix("/restore"))
        && method == "POST"
    {
        return Some(restore_profile_revision(ctx, rev));
    }

    if let Some(rest) = path.strip_prefix("/api/jobs/cvs/") {
        return Some(cv_by_id(ctx, rest));
    }

    if let Some(rest) = path.strip_prefix("/api/jobs/companies/") {
        let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        return match (method, parts.as_slice()) {
            ("GET", [id]) => Some(get_company(ctx, id)),
            ("PUT", [id, "no-luck"]) => Some(set_company_no_luck(ctx, id)),
            _ => None,
        };
    }

    if let Some(rest) = path.strip_prefix("/api/jobs/people/") {
        let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        return match (method, parts.as_slice()) {
            ("PATCH", [id]) => Some(update_person(ctx, id)),
            ("POST", [id, "draft"]) => Some(remake_person_draft(ctx, id)),
            ("PUT", [id, "draft"]) => Some(save_person_draft(ctx, id)),
            _ => None,
        };
    }

    if let Some(id) = path.strip_prefix("/api/jobs/postings/")
        && method == "PATCH"
    {
        return Some(update_posting(ctx, id.trim_matches('/')));
    }

    if let Some(name) = path.strip_prefix("/api/jobs/sources/telegram/") {
        let clean = url_decode_simple(name.trim_matches('/'));
        return match method {
            "PATCH" => Some(update_telegram_channel(ctx, &clean)),
            "DELETE" => Some(delete_telegram_channel(ctx, &clean)),
            _ => None,
        };
    }

    if let Some(key) = path.strip_prefix("/api/jobs/sources/switches/")
        && method == "PUT"
    {
        return Some(update_source_switch(ctx, &url_decode_simple(key.trim_matches('/'))));
    }

    if let Some(id) = path
        .strip_prefix("/api/jobs/helper/")
        .and_then(|s| s.strip_suffix("/revoke"))
        && method == "POST"
    {
        return Some(revoke_helper(ctx, id));
    }

    None
}

fn url_decode_simple(input: &str) -> String {
    input.replace("%3A", ":").replace("%3a", ":")
}

fn notify_jobs(ctx: &Ctx) {
    ctx.live.notify("jobs");
    ctx.refresh();
}

fn setup(ctx: &Ctx) -> Reply {
    let has_cv = ctx.store.count("cvs") > 0;
    let profile_doc = normalized_profile_doc(ctx.store.as_ref());
    let has_profile = profile_doc
        .pointer("/profile/name")
        .and_then(Value::as_str)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let templates = ctx.store.setting("job_templates");
    let has_templates = templates
        .map(|t| {
            !t.get("examples").and_then(Value::as_bool).unwrap_or(true)
                && t.get("templates")
                    .and_then(Value::as_array)
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
        })
        .unwrap_or(false);

    let main_cv = ctx
        .store
        .all("cvs")
        .into_iter()
        .find(|cv| cv.get("isMain").and_then(Value::as_bool).unwrap_or(false));

    Reply::ok(json!({
        "items": [
            {"key": "cv", "done": has_cv},
            {"key": "profile", "done": has_profile},
            {"key": "templates", "done": has_templates}
        ],
        "mainCv": main_cv.map(|cv| json!({
            "id": text(&cv, "id"),
            "label": text(&cv, "label"),
            "status": text(&cv, "status")
        }))
    }))
}

fn cvs(ctx: &Ctx) -> Reply {
    let cvs: Vec<Value> = ctx
        .store
        .all("cvs")
        .into_iter()
        .filter(|cv| !cv.get("archived").and_then(Value::as_bool).unwrap_or(false))
        .collect();
    Reply::ok(Value::Array(cvs))
}

const CV_MIME_PDF: &str = "application/pdf";
const CV_MIME_DOCX: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

fn is_allowed_cv_mime(mime: &str) -> bool {
    let base = mime.split(';').next().unwrap_or("").trim();
    base == CV_MIME_PDF || base == CV_MIME_DOCX
}

fn upload_cv(ctx: &Ctx) -> Reply {
    let id = uuid();
    let label = text(&ctx.body, "label").trim().to_string();
    if label.chars().count() > 160 {
        return Reply::error(400, "CV label must be 160 characters or fewer.");
    }
    let markdown = text(&ctx.body, "markdown");
    if markdown.chars().count() > 200_000 {
        return Reply::error(400, "CV markdown content is too large.");
    }
    let upload_id = text(&ctx.body, "uploadId");
    let file_meta = if !upload_id.is_empty() {
        ctx.store.get("artifacts", &upload_id)
    } else {
        ctx.body.get("file").cloned()
    };
    if let Some(ref meta) = file_meta {
        let mime = text(meta, "mimeType");
        if !mime.is_empty() && !is_allowed_cv_mime(&mime) {
            return Reply::error(400, "Upload a PDF or DOCX CV.");
        }
    }
    if file_meta.is_none() && markdown.trim().is_empty() {
        return Reply::error(400, "Provide a PDF/DOCX file or CV text.");
    }
    let now = iso_now();
    let cv = json!({
        "id": id,
        "label": if label.is_empty() { "Uploaded CV".to_string() } else { label },
        "isMain": ctx.store.all("cvs").into_iter().filter(|c| !c.get("archived").and_then(Value::as_bool).unwrap_or(false)).count() == 0,
        "status": "ready",
        "revision": 1,
        "markdown": markdown,
        "file": file_meta.unwrap_or(Value::Null),
        "pdf": null,
        "pdfFile": "",
        "converting": false,
        "createdAt": now,
        "updatedAt": now
    });
    let _ = ctx.store.put("cvs", &id, &cv);
    record_cv_revision(ctx.store, &id, &cv);
    notify_jobs(ctx);
    Reply::ok(json!({ "cv": cv, "conversion": "ready" }))
}

fn record_cv_revision(store: &Store, cv_id: &str, cv: &Value) {
    let rev_id = uuid();
    let _ = store.put(
        "job_cv_revisions",
        &rev_id,
        &json!({
            "id": rev_id,
            "cvId": cv_id,
            "revision": cv.get("revision").cloned().unwrap_or(json!(1)),
            "actor": "owner",
            "createdAt": iso_now(),
            "data": {
                "label": cv.get("label").cloned().unwrap_or(json!("")),
                "markdown": cv.get("markdown").cloned().unwrap_or(json!(""))
            }
        }),
    );
}

fn cv_by_id(ctx: &Ctx, rest: &str) -> Reply {
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let id = parts.first().copied().unwrap_or("");
    let action = parts.get(1).copied();
    match (ctx.method, action, parts.get(2).copied(), parts.get(3).copied()) {
        ("GET", None, _, _) => match ctx.store.get("cvs", id) {
            Some(cv) => Reply::ok(cv),
            None => Reply::error(404, "CV not found."),
        },
        ("PATCH", None, _, _) => match ctx.store.patch("cvs", id, &ctx.body) {
            Some(mut cv) => {
                if let Some(obj) = cv.as_object_mut() {
                    let rev = obj.get("revision").and_then(Value::as_i64).unwrap_or(1);
                    obj.insert("revision".to_string(), json!(rev + 1));
                    obj.insert("updatedAt".to_string(), json!(iso_now()));
                }
                let _ = ctx.store.put("cvs", id, &cv);
                record_cv_revision(ctx.store, id, &cv);
                notify_jobs(ctx);
                Reply::ok(cv)
            }
            None => Reply::error(404, "CV not found."),
        },
        ("POST", Some("main"), _, _) => {
            let Some(_) = ctx.store.get("cvs", id) else {
                return Reply::error(404, "CV not found.");
            };
            for mut item in ctx.store.all("cvs") {
                let item_id = text(&item, "id");
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("isMain".to_string(), json!(item_id == id));
                }
                let _ = ctx.store.put("cvs", &item_id, &item);
            }
            notify_jobs(ctx);
            Reply::ok(json!({ "ok": true }))
        }
        ("POST", Some("archive"), _, _) => {
            let Some(mut cv) = ctx.store.get("cvs", id) else {
                return Reply::error(404, "CV not found.");
            };
            if let Some(obj) = cv.as_object_mut() {
                obj.insert("archived".to_string(), json!(true));
                obj.insert("isMain".to_string(), json!(false));
            }
            let _ = ctx.store.put("cvs", id, &cv);
            notify_jobs(ctx);
            Reply::ok(json!({ "ok": true }))
        }
        ("POST", Some("convert"), _, _) => Reply::ok(json!({ "ok": true, "conversion": "ready" })),
        ("GET", Some("revisions"), _, _) => {
            let mut revs: Vec<Value> = ctx
                .store
                .all("job_cv_revisions")
                .into_iter()
                .filter(|r| r.get("cvId").and_then(Value::as_str) == Some(id))
                .collect();
            revs.reverse();
            Reply::ok(Value::Array(revs))
        }
        ("POST", Some("revisions"), Some(rev_id), Some("restore")) => {
            let Some(rev) = ctx.store.get("job_cv_revisions", rev_id) else {
                return Reply::error(404, "Revision not found.");
            };
            let Some(mut cv) = ctx.store.get("cvs", id) else {
                return Reply::error(404, "CV not found.");
            };
            if let Some(obj) = cv.as_object_mut() {
                if let Some(lbl) = rev.pointer("/data/label") {
                    obj.insert("label".to_string(), lbl.clone());
                }
                if let Some(md) = rev.pointer("/data/markdown") {
                    obj.insert("markdown".to_string(), md.clone());
                }
                let next_rev = obj.get("revision").and_then(Value::as_i64).unwrap_or(1) + 1;
                obj.insert("revision".to_string(), json!(next_rev));
                obj.insert("updatedAt".to_string(), json!(iso_now()));
            }
            let _ = ctx.store.put("cvs", id, &cv);
            notify_jobs(ctx);
            Reply::ok(cv)
        }
        ("DELETE", None, _, _) => {
            ctx.store.delete("cvs", id);
            notify_jobs(ctx);
            Reply::ok(json!({ "ok": true }))
        }
        _ => Reply::error(404, "CV not found."),
    }
}

fn default_job_profile() -> Value {
    json!({
        "profile": {
            "name": "",
            "intro": "",
            "location": "",
            "targetRoles": [],
            "regions": [],
            "languages": [],
            "languageRule": {"default": "en", "byCountry": {}},
            "programmes": [],
            "workAuthorization": "",
            "links": [],
            "inviteLimits": {"daily": 15, "weekly": 80},
            "noLuck": {"noAcceptDays": 7, "noReplyDays": 5},
            "waves": {"size": 5, "max": 10}
        },
        "portals": {},
        "revision": 1
    })
}

fn merge_missing_json(target: &mut Value, defaults: &Value) {
    if let (Some(t), Some(d)) = (target.as_object_mut(), defaults.as_object()) {
        for (key, default_val) in d {
            match t.get_mut(key) {
                None => {
                    t.insert(key.clone(), default_val.clone());
                }
                Some(existing) if existing.is_object() && default_val.is_object() => {
                    merge_missing_json(existing, default_val);
                }
                _ => {}
            }
        }
    }
}

fn normalized_profile_doc(store: &Store) -> Value {
    let mut doc = store.setting("job_profile").unwrap_or_else(default_job_profile);
    merge_missing_json(&mut doc, &default_job_profile());
    doc
}

fn limits_from_profile(store: &Store) -> Value {
    let doc = normalized_profile_doc(store);
    let no_accept = doc
        .pointer("/profile/noLuck/noAcceptDays")
        .and_then(Value::as_i64)
        .unwrap_or(7);
    let no_reply = doc
        .pointer("/profile/noLuck/noReplyDays")
        .and_then(Value::as_i64)
        .unwrap_or(5);
    let wave_size = doc
        .pointer("/profile/waves/size")
        .and_then(Value::as_i64)
        .unwrap_or(5);
    let wave_max = doc
        .pointer("/profile/waves/max")
        .and_then(Value::as_i64)
        .unwrap_or(10);
    json!({
        "noAcceptDays": no_accept,
        "noReplyDays": no_reply,
        "waves": { "size": wave_size, "max": wave_max }
    })
}

fn profile(ctx: &Ctx) -> Reply {
    Reply::ok(normalized_profile_doc(ctx.store.as_ref()))
}

const JOB_PLACEHOLDERS: &[&str] = &[
    "greeting",
    "first_name",
    "company",
    "programme",
    "track",
    "role",
    "my_name",
    "my_intro",
];

fn is_valid_language_tag(raw: &str) -> bool {
    let trimmed = raw.trim();
    let mut parts = trimmed.split('-');
    let Some(first) = parts.next() else {
        return false;
    };
    if !(2..=3).contains(&first.len()) || !first.bytes().all(|b| b.is_ascii_lowercase()) {
        return false;
    }
    for sub in parts {
        if !(2..=8).contains(&sub.len()) || !sub.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return false;
        }
    }
    true
}

fn is_valid_country_code(code: &str) -> bool {
    code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase())
}

fn is_valid_greeting_key(key: &str) -> bool {
    (1..=30).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

fn check_trimmed_text(val: &Value, field: &str, max_len: usize) -> Result<String, String> {
    let Some(raw) = val.as_str() else {
        return Err(format!("{field} must be a string."));
    };
    let trimmed = raw.trim();
    if trimmed.chars().count() > max_len {
        return Err(format!("{field} must be at most {max_len} characters."));
    }
    Ok(trimmed.to_string())
}

fn check_trimmed_item_list(val: &Value, field: &str, max_items: usize, max_len: usize) -> Result<Vec<Value>, String> {
    let Some(arr) = val.as_array() else {
        return Err(format!("{field} must be a list."));
    };
    if arr.len() > max_items {
        return Err(format!("{field} accepts at most {max_items} items."));
    }
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let s = check_trimmed_text(entry, field, max_len)?;
        if s.is_empty() {
            return Err(format!("{field} items cannot be empty."));
        }
        out.push(json!(s));
    }
    Ok(out)
}

fn check_int_bound(val: &Value, field: &str, min: i64, max: i64) -> Result<i64, String> {
    let Some(n) = val.as_i64() else {
        return Err(format!("{field} must be a whole number."));
    };
    if n < min || n > max {
        return Err(format!("{field} must be between {min} and {max}."));
    }
    Ok(n)
}

fn is_valid_http_url(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 500 || trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    let Some(rest) = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
    else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty() && !host.contains('\\')
}

fn validate_strict_keys(obj: &serde_json::Map<String, Value>, allowed: &[&str], scope: &str) -> Result<(), String> {
    for k in obj.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(format!("Unexpected field '{k}' in {scope}."));
        }
    }
    Ok(())
}

fn validate_and_normalize_profile_patch(raw: &Value) -> Result<Value, String> {
    let Some(obj) = raw.as_object() else {
        return Err("Profile patch must be an object.".to_string());
    };
    validate_strict_keys(
        obj,
        &[
            "name",
            "intro",
            "location",
            "targetRoles",
            "regions",
            "languages",
            "languageRule",
            "programmes",
            "workAuthorization",
            "links",
            "inviteLimits",
            "noLuck",
            "waves",
        ],
        "profile",
    )?;
    let mut clean = serde_json::Map::new();
    for (k, v) in obj {
        match k.as_str() {
            "name" => {
                clean.insert(k.clone(), json!(check_trimmed_text(v, "name", 120)?));
            }
            "intro" => {
                clean.insert(k.clone(), json!(check_trimmed_text(v, "intro", 600)?));
            }
            "location" => {
                clean.insert(k.clone(), json!(check_trimmed_text(v, "location", 200)?));
            }
            "workAuthorization" => {
                clean.insert(
                    k.clone(),
                    json!(check_trimmed_text(v, "workAuthorization", 400)?),
                );
            }
            "targetRoles" => {
                clean.insert(k.clone(), Value::Array(check_trimmed_item_list(v, "targetRoles", 30, 120)?));
            }
            "regions" => {
                clean.insert(k.clone(), Value::Array(check_trimmed_item_list(v, "regions", 30, 120)?));
            }
            "languages" => {
                clean.insert(k.clone(), Value::Array(check_trimmed_item_list(v, "languages", 20, 60)?));
            }
            "languageRule" => {
                let Some(lr) = v.as_object() else {
                    return Err("languageRule must be an object.".to_string());
                };
                validate_strict_keys(lr, &["default", "byCountry"], "languageRule")?;
                let mut lr_clean = serde_json::Map::new();
                if let Some(def_val) = lr.get("default") {
                    let s = check_trimmed_text(def_val, "languageRule.default", 32)?;
                    if !is_valid_language_tag(&s) {
                        return Err("Use a language tag such as en or fr-CA.".to_string());
                    }
                    lr_clean.insert("default".to_string(), json!(s));
                }
                if let Some(bc_val) = lr.get("byCountry") {
                    let Some(bc_map) = bc_val.as_object() else {
                        return Err("languageRule.byCountry must be an object.".to_string());
                    };
                    let mut bc_clean = serde_json::Map::new();
                    for (cc, lang_val) in bc_map {
                        if !is_valid_country_code(cc) {
                            return Err("Use a two-letter country code such as CA.".to_string());
                        }
                        let lang = check_trimmed_text(lang_val, "languageRule.byCountry", 32)?;
                        if !is_valid_language_tag(&lang) {
                            return Err("Use a language tag such as en or fr-CA.".to_string());
                        }
                        bc_clean.insert(cc.clone(), json!(lang));
                    }
                    lr_clean.insert("byCountry".to_string(), Value::Object(bc_clean));
                }
                clean.insert(k.clone(), Value::Object(lr_clean));
            }
            "programmes" => {
                let Some(arr) = v.as_array() else {
                    return Err("programmes must be a list.".to_string());
                };
                if arr.len() > 50 {
                    return Err("programmes accepts at most 50 items.".to_string());
                }
                let mut items = Vec::with_capacity(arr.len());
                for p in arr {
                    let Some(po) = p.as_object() else {
                        return Err("Each programme must be an object.".to_string());
                    };
                    validate_strict_keys(po, &["name", "tracks"], "programme")?;
                    let pname = check_trimmed_text(
                        po.get("name").unwrap_or(&Value::Null),
                        "programme.name",
                        160,
                    )?;
                    if pname.is_empty() {
                        return Err("Programme name cannot be empty.".to_string());
                    }
                    let tracks = check_trimmed_item_list(
                        po.get("tracks").unwrap_or(&Value::Null),
                        "programme.tracks",
                        50,
                        160,
                    )?;
                    items.push(json!({ "name": pname, "tracks": tracks }));
                }
                clean.insert(k.clone(), Value::Array(items));
            }
            "links" => {
                let Some(arr) = v.as_array() else {
                    return Err("links must be a list.".to_string());
                };
                if arr.len() > 20 {
                    return Err("links accepts at most 20 items.".to_string());
                }
                let mut items = Vec::with_capacity(arr.len());
                for l in arr {
                    let Some(lo) = l.as_object() else {
                        return Err("Each link must be an object.".to_string());
                    };
                    validate_strict_keys(lo, &["label", "url"], "link")?;
                    let label = check_trimmed_text(
                        lo.get("label").unwrap_or(&Value::Null),
                        "link.label",
                        60,
                    )?;
                    if label.is_empty() {
                        return Err("Link label cannot be empty.".to_string());
                    }
                    let url_raw = lo.get("url").and_then(Value::as_str).unwrap_or("");
                    if !is_valid_http_url(url_raw) {
                        return Err("Enter a valid URL for link.".to_string());
                    }
                    items.push(json!({ "label": label, "url": url_raw.trim() }));
                }
                clean.insert(k.clone(), Value::Array(items));
            }
            "inviteLimits" => {
                let Some(il) = v.as_object() else {
                    return Err("inviteLimits must be an object.".to_string());
                };
                validate_strict_keys(il, &["daily", "weekly"], "inviteLimits")?;
                let mut il_clean = serde_json::Map::new();
                if let Some(d) = il.get("daily") {
                    il_clean.insert("daily".to_string(), json!(check_int_bound(d, "inviteLimits.daily", 0, 100)?));
                }
                if let Some(w) = il.get("weekly") {
                    il_clean.insert("weekly".to_string(), json!(check_int_bound(w, "inviteLimits.weekly", 0, 500)?));
                }
                clean.insert(k.clone(), Value::Object(il_clean));
            }
            "noLuck" => {
                let Some(nl) = v.as_object() else {
                    return Err("noLuck must be an object.".to_string());
                };
                validate_strict_keys(nl, &["noAcceptDays", "noReplyDays"], "noLuck")?;
                let mut nl_clean = serde_json::Map::new();
                if let Some(a) = nl.get("noAcceptDays") {
                    nl_clean.insert("noAcceptDays".to_string(), json!(check_int_bound(a, "noLuck.noAcceptDays", 1, 90)?));
                }
                if let Some(r) = nl.get("noReplyDays") {
                    nl_clean.insert("noReplyDays".to_string(), json!(check_int_bound(r, "noLuck.noReplyDays", 1, 90)?));
                }
                clean.insert(k.clone(), Value::Object(nl_clean));
            }
            "waves" => {
                let Some(wv) = v.as_object() else {
                    return Err("waves must be an object.".to_string());
                };
                validate_strict_keys(wv, &["size", "max"], "waves")?;
                let mut wv_clean = serde_json::Map::new();
                if let Some(s) = wv.get("size") {
                    wv_clean.insert("size".to_string(), json!(check_int_bound(s, "waves.size", 1, 50)?));
                }
                if let Some(m) = wv.get("max") {
                    wv_clean.insert("max".to_string(), json!(check_int_bound(m, "waves.max", 1, 200)?));
                }
                clean.insert(k.clone(), Value::Object(wv_clean));
            }
            _ => {}
        }
    }
    Ok(Value::Object(clean))
}

fn unknown_placeholders(body: &str) -> Vec<String> {
    let mut bad = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            break;
        };
        let candidate = &after[..end];
        if !candidate.contains('{') {
            if !JOB_PLACEHOLDERS.contains(&candidate) && !bad.iter().any(|s| s == candidate) {
                bad.push(candidate.to_string());
            }
            rest = &after[end + 1..];
        } else {
            rest = after;
        }
    }
    bad
}

fn update_profile(ctx: &Ctx) -> Reply {
    let Some(body_obj) = ctx.body.as_object() else {
        return Reply::error(400, "Invalid profile payload.");
    };
    if let Err(err) = validate_strict_keys(body_obj, &["profile", "portals", "revision"], "job_profile") {
        return Reply::error(400, err);
    }
    let mut validated_patch = serde_json::Map::new();
    if let Some(profile_val) = body_obj.get("profile") {
        match validate_and_normalize_profile_patch(profile_val) {
            Ok(clean) => {
                validated_patch.insert("profile".to_string(), clean);
            }
            Err(err) => return Reply::error(400, err),
        }
    }
    if let Some(portals_val) = body_obj.get("portals") {
        let Some(portals_map) = portals_val.as_object() else {
            return Reply::error(400, "portals must be an object.");
        };
        for key in portals_map.keys() {
            if key.is_empty() || key.chars().count() > 80 {
                return Reply::error(400, "Portal keys must be 1 to 80 characters.");
            }
        }
        validated_patch.insert("portals".to_string(), portals_val.clone());
    }

    let current = normalized_profile_doc(ctx.store.as_ref());
    let next_rev = current.get("revision").and_then(Value::as_i64).unwrap_or(1) + 1;
    let mut updated = ctx
        .store
        .update_setting("job_profile", &Value::Object(validated_patch));
    merge_missing_json(&mut updated, &default_job_profile());
    if let Some(obj) = updated.as_object_mut() {
        obj.insert("revision".to_string(), json!(next_rev));
    }
    ctx.store.set_setting("job_profile", &updated);
    let rev_id = uuid();
    let _ = ctx.store.put(
        "job_profile_revisions",
        &rev_id,
        &json!({
            "id": rev_id,
            "revision": next_rev,
            "actor": "owner",
            "createdAt": iso_now(),
            "data": { "profile": updated.get("profile").cloned().unwrap_or_else(|| json!({})) }
        }),
    );
    notify_jobs(ctx);
    Reply::ok(updated)
}

fn profile_revisions(ctx: &Ctx) -> Reply {
    let mut revs = ctx.store.all("job_profile_revisions");
    revs.reverse();
    Reply::ok(Value::Array(revs))
}

fn restore_profile_revision(ctx: &Ctx, rev_id: &str) -> Reply {
    let Some(rev) = ctx.store.get("job_profile_revisions", rev_id) else {
        return Reply::error(404, "Revision not found.");
    };
    let mut current = normalized_profile_doc(ctx.store.as_ref());
    let next_rev = current.get("revision").and_then(Value::as_i64).unwrap_or(1) + 1;
    if let Some(obj) = current.as_object_mut() {
        if let Some(p) = rev.pointer("/data/profile") {
            obj.insert("profile".to_string(), p.clone());
        }
        obj.insert("revision".to_string(), json!(next_rev));
    }
    merge_missing_json(&mut current, &default_job_profile());
    ctx.store.set_setting("job_profile", &current);
    notify_jobs(ctx);
    Reply::ok(current)
}

fn templates(ctx: &Ctx) -> Reply {
    let templates = ctx.store.setting("job_templates").unwrap_or_else(|| {
        json!({"templates": [], "examples": true, "revision": 1})
    });
    Reply::ok(templates)
}

fn update_templates(ctx: &Ctx) -> Reply {
    let current = ctx
        .store
        .setting("job_templates")
        .unwrap_or_else(|| json!({"templates": [], "examples": true, "revision": 1}));
    let next_rev = current.get("revision").and_then(Value::as_i64).unwrap_or(1) + 1;
    let Some(list) = ctx.body.get("templates").and_then(Value::as_array) else {
        return Reply::error(400, "templates must be a list.");
    };
    let mut cleaned = Vec::with_capacity(list.len());
    for item in list {
        let Some(obj) = item.as_object() else {
            return Reply::error(400, "Each template must be an object.");
        };
        if let Err(err) = validate_strict_keys(obj, &["language", "body", "greetings"], "template") {
            return Reply::error(400, err);
        }
        let lang = match check_trimmed_text(
            obj.get("language").unwrap_or(&Value::Null),
            "template.language",
            32,
        ) {
            Ok(s) if is_valid_language_tag(&s) => s,
            Ok(_) => return Reply::error(400, "Use a language tag such as en or fr-CA."),
            Err(err) => return Reply::error(400, err),
        };
        let body = match check_trimmed_text(
            obj.get("body").unwrap_or(&Value::Null),
            "template.body",
            4000,
        ) {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => return Reply::error(400, "Enter template text."),
            Err(err) => return Reply::error(400, err),
        };
        let bad_placeholders = unknown_placeholders(&body);
        if !bad_placeholders.is_empty() {
            return Reply::error(
                400,
                format!("Unknown template placeholders: {}", bad_placeholders.join(", ")),
            );
        }
        let Some(gmap) = obj.get("greetings").and_then(Value::as_object) else {
            return Reply::error(400, "template.greetings must be an object.");
        };
        let mut gclean = serde_json::Map::new();
        for (gk, gv) in gmap {
            if !is_valid_greeting_key(gk) {
                return Reply::error(400, "Greeting keys must use lowercase letters or underscores.");
            }
            match check_trimmed_text(gv, "greeting", 80) {
                Ok(s) if !s.is_empty() => {
                    gclean.insert(gk.clone(), json!(s));
                }
                Ok(_) => return Reply::error(400, "Greeting value cannot be empty."),
                Err(err) => return Reply::error(400, err),
            }
        }
        cleaned.push(json!({
            "language": lang,
            "body": body,
            "greetings": Value::Object(gclean),
        }));
    }
    let updated = json!({
        "templates": cleaned,
        "examples": false,
        "revision": next_rev
    });
    ctx.store.set_setting("job_templates", &updated);
    notify_jobs(ctx);
    Reply::ok(updated)
}

fn invite_budget(store: &Store) -> (i64, i64) {
    let doc = normalized_profile_doc(store);
    let daily = doc
        .pointer("/profile/inviteLimits/daily")
        .and_then(Value::as_i64)
        .unwrap_or(15);
    let weekly = doc
        .pointer("/profile/inviteLimits/weekly")
        .and_then(Value::as_i64)
        .unwrap_or(80);
    (daily, weekly)
}

fn compute_company_view_fields(store: &Store, company: &Value, all_people: &[Value], all_postings: &[Value]) -> Value {
    let id = text(company, "id");
    let company_name_lower = text(company, "name").trim().to_ascii_lowercase();
    let mut invited = 0_i64;
    let mut accepted = 0_i64;
    let mut asked = 0_i64;
    let mut replied = 0_i64;
    let mut referred = 0_i64;
    let mut last_activity: Option<String> = company
        .get("lastActivityAt")
        .and_then(Value::as_str)
        .map(str::to_string);

    for person in all_people {
        if text(person, "companyId") != id {
            continue;
        }
        let status = text(person, "status");
        match status.as_str() {
            "invited" => invited += 1,
            "accepted" => accepted += 1,
            "asked" => asked += 1,
            "replied" => replied += 1,
            "referred" => referred += 1,
            _ => {}
        }
        let updated = text(person, "updatedAt");
        if !updated.is_empty() && last_activity.as_deref().map(|l| updated.as_str() > l).unwrap_or(true) {
            last_activity = Some(updated);
        }
    }

    let mut open_postings = 0_i64;
    let mut best_score: Option<f64> = None;
    for posting in all_postings {
        let same_company = text(posting, "companyId") == id
            || text(posting, "company").trim().to_ascii_lowercase() == company_name_lower;
        if !same_company || text(posting, "status") == "skipped" {
            continue;
        }
        open_postings += 1;
        if let Some(score) = posting.get("score").and_then(Value::as_f64) {
            best_score = Some(best_score.map_or(score, |b| b.max(score)));
        }
    }

    let (daily, weekly) = invite_budget(store);
    let limits = limits_from_profile(store);
    let size = limits.pointer("/waves/size").and_then(Value::as_i64).unwrap_or(5);
    let max = limits.pointer("/waves/max").and_then(Value::as_i64).unwrap_or(10);
    let total_people = invited + accepted + asked + replied + referred;
    let wave = if total_people == 0 || size <= 0 {
        1
    } else {
        ((total_people - 1) / size) + 1
    };
    let in_wave = if total_people == 0 || size <= 0 {
        0
    } else {
        let rem = total_people % size;
        if rem == 0 { size } else { rem }
    };
    let left = (max - total_people).max(0);
    let more = (size - in_wave).max(0).min(left);

    let choice = company
        .pointer("/noLuck/choice")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    let explicit_val = company
        .pointer("/noLuck/value")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let no_luck_value = match choice {
        "no_luck" => true,
        "keep_trying" => false,
        _ => explicit_val && referred == 0,
    };
    let wave_state = if no_luck_value {
        "stopped"
    } else if total_people >= max {
        "done"
    } else if total_people > 0 && in_wave >= size && invited > 0 {
        "waiting"
    } else if total_people > 0 && in_wave >= size && invited == 0 {
        "next"
    } else {
        "open"
    };

    json!({
        "id": id,
        "name": text(company, "name"),
        "linkedinUrl": company.get("linkedinUrl").cloned().unwrap_or(Value::Null),
        "careersUrl": company.get("careersUrl").cloned().unwrap_or(Value::Null),
        "openPostings": open_postings,
        "bestScore": best_score,
        "counts": {
            "invited": invited,
            "accepted": accepted,
            "asked": asked,
            "replied": replied,
            "referred": referred,
        },
        "noLuck": {
            "value": no_luck_value,
            "by": company.pointer("/noLuck/by").cloned().unwrap_or(Value::Null),
            "since": company.pointer("/noLuck/since").cloned().unwrap_or(Value::Null),
            "choice": choice,
        },
        "applyChat": company.get("applyChat").cloned().unwrap_or(Value::Null),
        "waves": {
            "state": wave_state,
            "wave": wave,
            "inWave": in_wave,
            "left": left,
            "more": more,
            "invited": invited,
            "size": size,
            "max": max,
            "today": 0,
            "week": total_people,
            "daily": daily,
            "weekly": weekly,
            "over": Value::Null,
            "until": Value::Null,
            "owed": 0
        },
        "revision": company.get("revision").and_then(Value::as_i64).unwrap_or(1),
        "lastActivityAt": last_activity,
    })
}

fn company_matches_filter(company: &Value, filter: &str) -> bool {
    let no_luck = company.pointer("/noLuck/value").and_then(Value::as_bool).unwrap_or(false);
    let invited = company.pointer("/counts/invited").and_then(Value::as_i64).unwrap_or(0);
    let accepted = company.pointer("/counts/accepted").and_then(Value::as_i64).unwrap_or(0);
    let asked = company.pointer("/counts/asked").and_then(Value::as_i64).unwrap_or(0);
    let wave_state = company.pointer("/waves/state").and_then(Value::as_str).unwrap_or("open");
    match filter {
        "waiting" => invited > 0 && !no_luck,
        "to_ask" => accepted > 0 && !no_luck,
        "asked" => asked > 0 && !no_luck,
        "next_wave" => wave_state == "next" && !no_luck,
        "no_luck" => no_luck,
        _ => true,
    }
}

fn list_companies(ctx: &Ctx) -> Reply {
    let filter = ctx.param("filter").unwrap_or_else(|| "all".to_string());
    let limit = ctx
        .param("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    let all_companies = ctx.store.all("job_companies");
    let all_people = ctx.store.all("job_people");
    let all_postings = ctx.store.all("job_postings");

    let mut hydrated: Vec<Value> = all_companies
        .iter()
        .map(|c| compute_company_view_fields(ctx.store.as_ref(), c, &all_people, &all_postings))
        .collect();
    hydrated.sort_by_key(|a| std::cmp::Reverse(text(a, "lastActivityAt")));

    let counts = json!({
        "all": hydrated.len(),
        "waiting": hydrated.iter().filter(|c| company_matches_filter(c, "waiting")).count(),
        "to_ask": hydrated.iter().filter(|c| company_matches_filter(c, "to_ask")).count(),
        "asked": hydrated.iter().filter(|c| company_matches_filter(c, "asked")).count(),
        "next_wave": hydrated.iter().filter(|c| company_matches_filter(c, "next_wave")).count(),
        "no_luck": hydrated.iter().filter(|c| company_matches_filter(c, "no_luck")).count(),
    });
    let filtered: Vec<Value> = hydrated
        .into_iter()
        .filter(|c| company_matches_filter(c, &filter))
        .collect();
    let total = filtered.len();
    let visible: Vec<Value> = filtered.into_iter().take(limit).collect();
    let (daily, weekly) = invite_budget(ctx.store.as_ref());

    Reply::ok(json!({
        "companies": visible,
        "total": total,
        "counts": counts,
        "limits": limits_from_profile(ctx.store.as_ref()),
        "invites": {
            "today": 0,
            "week": all_people.len(),
            "daily": daily,
            "weekly": weekly
        }
    }))
}

fn build_company_detail(store: &Store, company: &Value) -> Value {
    let all_people = store.all("job_people");
    let all_postings = store.all("job_postings");
    let mut detail = compute_company_view_fields(store, company, &all_people, &all_postings);
    let cid = text(company, "id");
    let cname_lower = text(company, "name").trim().to_ascii_lowercase();
    let people: Vec<Value> = all_people
        .into_iter()
        .filter(|p| text(p, "companyId") == cid)
        .collect();
    let postings: Vec<Value> = all_postings
        .into_iter()
        .filter(|p| {
            text(p, "companyId") == cid
                || text(p, "company").trim().to_ascii_lowercase() == cname_lower
        })
        .collect();
    if let Some(obj) = detail.as_object_mut() {
        obj.insert("people".to_string(), Value::Array(people));
        obj.insert("postings".to_string(), Value::Array(postings));
        obj.insert("limits".to_string(), limits_from_profile(store));
    }
    detail
}

fn get_company(ctx: &Ctx, id: &str) -> Reply {
    let Some(company) = ctx.store.get("job_companies", id) else {
        return Reply::error(404, "Company not found.");
    };
    Reply::ok(build_company_detail(ctx.store.as_ref(), &company))
}

fn set_company_no_luck(ctx: &Ctx, id: &str) -> Reply {
    let Some(mut company) = ctx.store.get("job_companies", id) else {
        return Reply::error(404, "Company not found.");
    };
    let current_rev = company.get("revision").and_then(Value::as_i64).unwrap_or(1);
    if let Some(expected_rev) = ctx.body.get("revision").and_then(Value::as_i64)
        && expected_rev != current_rev
    {
        return Reply::error(409, "This company changed elsewhere. Reload and try again.");
    }

    let now = iso_now();
    let (choice, value, by, since) = match ctx.body.get("noLuck") {
        Some(Value::Bool(true)) => ("no_luck", true, json!("owner"), json!(now)),
        Some(Value::Bool(false)) => ("keep_trying", false, json!("owner"), Value::Null),
        _ => ("auto", false, Value::Null, Value::Null),
    };
    if let Some(obj) = company.as_object_mut() {
        obj.insert(
            "noLuck".to_string(),
            json!({
                "value": value,
                "by": by,
                "since": since,
                "choice": choice
            }),
        );
        obj.insert("revision".to_string(), json!(current_rev + 1));
        obj.insert("lastActivityAt".to_string(), json!(now));
    }
    if let Err(err) = ctx.store.put("job_companies", id, &company) {
        return Reply::error(500, err);
    }
    notify_jobs(ctx);
    Reply::ok(json!({
        "company": build_company_detail(ctx.store.as_ref(), &company),
        "apply": Value::Null
    }))
}

fn update_person(ctx: &Ctx, id: &str) -> Reply {
    let Some(mut person) = ctx.store.get("job_people", id) else {
        return Reply::error(404, "Person not found.");
    };
    let current_rev = person.get("revision").and_then(Value::as_i64).unwrap_or(1);
    if let Some(expected_rev) = ctx.body.get("revision").and_then(Value::as_i64)
        && expected_rev != current_rev
    {
        return Reply::error(409, "This person changed elsewhere. Reload and try again.");
    }

    if let Some(status) = ctx.body.get("status").and_then(Value::as_str)
        && !matches!(
            status,
            "invited" | "accepted" | "asked" | "replied" | "referred" | "declined"
        )
    {
        return Reply::error(400, "Choose a valid person status.");
    }
    if let Some(lang) = ctx.body.get("language").and_then(Value::as_str)
        && !lang.trim().is_empty()
        && !is_valid_language_tag(lang)
    {
        return Reply::error(400, "Use a language tag such as en or fr-CA.");
    }

    let now = iso_now();
    if let Some(obj) = person.as_object_mut() {
        if let Some(status) = ctx.body.get("status").and_then(Value::as_str) {
            obj.insert("status".to_string(), json!(status));
            obj.insert("statusAt".to_string(), json!(now));
            let mut times = obj
                .get("times")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            times.insert(status.to_string(), json!(now));
            obj.insert("times".to_string(), Value::Object(times));
        }
        if let Some(lang) = ctx.body.get("language").and_then(Value::as_str) {
            obj.insert("language".to_string(), json!(lang.trim()));
        }
        if let Some(name) = ctx.body.get("name").and_then(Value::as_str) {
            obj.insert("name".to_string(), json!(name));
        }
        if let Some(headline) = ctx.body.get("headline").and_then(Value::as_str) {
            obj.insert("headline".to_string(), json!(headline));
        }
        obj.insert("revision".to_string(), json!(current_rev + 1));
        obj.insert("updatedAt".to_string(), json!(now));
    }
    if let Err(err) = ctx.store.put("job_people", id, &person) {
        return Reply::error(500, err);
    }
    notify_jobs(ctx);
    Reply::ok(person)
}

fn build_person_draft(store: &Store, person: &Value) -> (Value, Vec<String>) {
    let templates_doc = store
        .setting("job_templates")
        .unwrap_or_else(|| json!({ "templates": [], "examples": true, "revision": 1 }));
    let list = templates_doc
        .get("templates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let profile_doc = normalized_profile_doc(store);
    let default_lang = profile_doc
        .pointer("/profile/languageRule/default")
        .and_then(Value::as_str)
        .unwrap_or("en");
    let person_lang = text(person, "language");
    let chosen_lang = if person_lang.is_empty() {
        default_lang.to_string()
    } else {
        person_lang
    };
    let template = list
        .iter()
        .find(|t| text(t, "language").eq_ignore_ascii_case(&chosen_lang))
        .or_else(|| list.first())
        .cloned();

    let Some(tmpl) = template else {
        let missing = vec!["template".to_string()];
        return (
            json!({
                "text": "",
                "raw": "",
                "state": "missing",
                "reason": "Save a referral message template first.",
                "note": "",
                "early": false,
                "language": chosen_lang,
                "languageBy": "default",
                "templateRevision": templates_doc.get("revision").cloned().unwrap_or(json!(1)),
                "missing": missing,
                "editedBy": Value::Null,
                "at": iso_now(),
            }),
            missing,
        );
    };

    let body = text(&tmpl, "body");
    let first_name = text(person, "name")
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();
    let company_name = store
        .get("job_companies", &text(person, "companyId"))
        .map(|c| text(&c, "name"))
        .unwrap_or_default();
    let my_name = profile_doc
        .pointer("/profile/name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let my_intro = profile_doc
        .pointer("/profile/intro")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let role = profile_doc
        .pointer("/profile/targetRoles/0")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let greeting = tmpl
        .pointer("/greetings/default")
        .and_then(Value::as_str)
        .or_else(|| tmpl.pointer("/greetings/morning").and_then(Value::as_str))
        .unwrap_or("Hi")
        .to_string();

    let rendered = body
        .replace("{greeting}", &greeting)
        .replace("{first_name}", &first_name)
        .replace("{company}", &company_name)
        .replace("{role}", &role)
        .replace("{my_name}", &my_name)
        .replace("{my_intro}", &my_intro);

    let mut missing = Vec::new();
    for key in ["first_name", "company", "role", "my_name", "my_intro"] {
        if rendered.contains(&format!("{{{key}}}")) {
            missing.push(key.to_string());
        }
    }

    let state = if missing.is_empty() { "filled" } else { "missing" };
    (
        json!({
            "text": rendered,
            "raw": rendered,
            "state": state,
            "reason": "",
            "note": "",
            "early": false,
            "language": text(&tmpl, "language"),
            "languageBy": if text(person, "language").is_empty() { "default" } else { "owner" },
            "templateRevision": templates_doc.get("revision").cloned().unwrap_or(json!(1)),
            "missing": missing,
            "editedBy": "nod",
            "at": iso_now(),
        }),
        missing,
    )
}

fn remake_person_draft(ctx: &Ctx, id: &str) -> Reply {
    let Some(mut person) = ctx.store.get("job_people", id) else {
        return Reply::error(404, "Person not found.");
    };
    let current_draft_rev = person.get("draftRevision").and_then(Value::as_i64).unwrap_or(1);
    if let Some(expected) = ctx.body.get("draftRevision").and_then(Value::as_i64)
        && expected != current_draft_rev
    {
        return Reply::error(409, "This draft changed elsewhere. Reload and try again.");
    }

    let (draft_info, missing) = build_person_draft(ctx.store.as_ref(), &person);
    let now = iso_now();
    if let Some(obj) = person.as_object_mut() {
        obj.insert(
            "draft".to_string(),
            json!(draft_info.get("text").and_then(Value::as_str).unwrap_or("")),
        );
        obj.insert("draftInfo".to_string(), draft_info);
        obj.insert("draftRevision".to_string(), json!(current_draft_rev + 1));
        obj.insert("updatedAt".to_string(), json!(now));
    }
    if let Err(err) = ctx.store.put("job_people", id, &person) {
        return Reply::error(500, err);
    }
    notify_jobs(ctx);
    Reply::ok(json!({ "person": person, "missing": missing }))
}

fn save_person_draft(ctx: &Ctx, id: &str) -> Reply {
    let Some(mut person) = ctx.store.get("job_people", id) else {
        return Reply::error(404, "Person not found.");
    };
    let current_draft_rev = person.get("draftRevision").and_then(Value::as_i64).unwrap_or(1);
    if let Some(expected) = ctx.body.get("draftRevision").and_then(Value::as_i64)
        && expected != current_draft_rev
    {
        return Reply::error(409, "This draft changed elsewhere. Reload and try again.");
    }

    let body_text = text(&ctx.body, "text");
    let now = iso_now();
    let lang = {
        let existing = text(&person, "language");
        if existing.is_empty() {
            "en".to_string()
        } else {
            existing
        }
    };
    let mut draft_info = person
        .get("draftInfo")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Some(d) = draft_info.as_object_mut() {
        d.insert("text".to_string(), json!(body_text));
        d.insert("raw".to_string(), json!(body_text));
        d.insert("state".to_string(), json!("edited"));
        d.insert("editedBy".to_string(), json!("owner"));
        d.insert("language".to_string(), json!(lang));
        d.insert("at".to_string(), json!(now));
    }
    if let Some(obj) = person.as_object_mut() {
        obj.insert("draft".to_string(), json!(body_text));
        obj.insert("draftInfo".to_string(), draft_info);
        obj.insert("draftRevision".to_string(), json!(current_draft_rev + 1));
        obj.insert("updatedAt".to_string(), json!(now));
    }
    if let Err(err) = ctx.store.put("job_people", id, &person) {
        return Reply::error(500, err);
    }
    notify_jobs(ctx);
    Reply::ok(person)
}

fn list_postings(ctx: &Ctx) -> Reply {
    let status = ctx.param("status").unwrap_or_else(|| "new".to_string());
    let source = ctx.param("source").unwrap_or_default();
    let min_score = ctx.param("minScore").and_then(|s| s.parse::<f64>().ok());
    let limit = ctx
        .param("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 500);

    let mut all = ctx.store.all("job_postings");
    all.sort_by_key(|a| std::cmp::Reverse(text(a, "firstSeenAt")));

    let counts = json!({
        "new": all.iter().filter(|p| text(p, "status") == "new").count(),
        "shortlisted": all.iter().filter(|p| text(p, "status") == "shortlisted").count(),
        "skipped": all.iter().filter(|p| text(p, "status") == "skipped").count(),
    });

    let filtered: Vec<Value> = all
        .into_iter()
        .filter(|p| status == "all" || text(p, "status") == status)
        .filter(|p| {
            min_score
                .map(|min| {
                    p.get("score")
                        .and_then(Value::as_f64)
                        .map(|s| s >= min)
                        .unwrap_or(false)
                })
                .unwrap_or(true)
        })
        .filter(|p| {
            source.is_empty()
                || p.get("sources")
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().any(|s| text(s, "kind") == source))
                    .unwrap_or(false)
        })
        .collect();
    let total = filtered.len();
    let visible: Vec<Value> = filtered.into_iter().take(limit).collect();

    Reply::ok(json!({
        "postings": visible,
        "total": total,
        "counts": counts,
    }))
}

fn update_posting(ctx: &Ctx, id: &str) -> Reply {
    let Some(mut posting) = ctx.store.get("job_postings", id) else {
        return Reply::error(404, "Posting not found.");
    };
    let current_rev = posting.get("revision").and_then(Value::as_i64).unwrap_or(1);
    if let Some(expected_rev) = ctx.body.get("revision").and_then(Value::as_i64)
        && expected_rev != current_rev
    {
        return Reply::error(409, "This posting changed elsewhere. Reload and try again.");
    }
    let next_status = text(&ctx.body, "status");
    if !matches!(next_status.as_str(), "new" | "shortlisted" | "skipped") {
        return Reply::error(400, "Choose a valid posting status.");
    }
    if let Some(obj) = posting.as_object_mut() {
        obj.insert("status".to_string(), json!(next_status));
        obj.insert("revision".to_string(), json!(current_rev + 1));
        obj.insert("lastSeenAt".to_string(), json!(iso_now()));
    }
    if let Err(err) = ctx.store.put("job_postings", id, &posting) {
        return Reply::error(500, err);
    }
    notify_jobs(ctx);
    Reply::ok(posting)
}

fn sources_state(store: &Store) -> Value {
    let defaults = json!({
        "channels": [],
        "switches": [
            { "key": "scan", "enabled": true, "revision": 1 },
            { "key": "scan:full", "enabled": false, "revision": 1 },
            { "key": "email", "enabled": true, "revision": 1 },
        ],
        "boards": []
    });
    let mut state = store.setting("job_sources").unwrap_or(defaults.clone());
    merge_missing_json(&mut state, &defaults);
    state
}

fn list_sources(ctx: &Ctx) -> Reply {
    let state = sources_state(ctx.store.as_ref());
    let mut companies: BTreeSet<String> = BTreeSet::new();
    for company in ctx.store.all("job_companies") {
        companies.insert(text(&company, "id"));
    }
    Reply::ok(json!({
        "channels": state.get("channels").cloned().unwrap_or_else(|| json!([])),
        "switches": state.get("switches").cloned().unwrap_or_else(|| json!([])),
        "boards": state.get("boards").cloned().unwrap_or_else(|| json!([])),
        "companies": companies.len()
    }))
}

fn parse_telegram_channel_name(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.contains("joinchat") || trimmed.contains("/+") || trimmed.starts_with('+') {
        return Err("That's an invite link. Nod reads public channels only, by their name.");
    }
    let lower = trimmed.to_ascii_lowercase();
    let stripped = [
        "https://t.me/s/",
        "http://t.me/s/",
        "https://telegram.me/s/",
        "http://telegram.me/s/",
        "https://t.me/",
        "http://t.me/",
        "https://telegram.me/",
        "http://telegram.me/",
        "t.me/s/",
        "t.me/",
        "telegram.me/",
    ]
    .iter()
    .find_map(|prefix| lower.strip_prefix(prefix).map(|_| &trimmed[prefix.len()..]))
    .unwrap_or(trimmed);
    let no_at = stripped.strip_prefix('@').unwrap_or(stripped);
    let clean = no_at
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    let valid = clean.len() >= 5
        && clean.len() <= 32
        && clean.starts_with(|c: char| c.is_ascii_alphabetic())
        && clean.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        Ok(clean.to_string())
    } else {
        Err("Use a public channel name such as example_jobs, @example_jobs or https://t.me/example_jobs.")
    }
}

fn add_telegram_channel(ctx: &Ctx) -> Reply {
    let name = match parse_telegram_channel_name(&text(&ctx.body, "name")) {
        Ok(name) => name,
        Err(msg) => return Reply::error(400, msg),
    };
    let mut state = sources_state(ctx.store.as_ref());
    let channels = state
        .get_mut("channels")
        .and_then(Value::as_array_mut);
    let Some(list) = channels else {
        return Reply::error(500, "Could not update job sources.");
    };
    if list.iter().any(|c| text(c, "name").eq_ignore_ascii_case(&name)) {
        return Reply::error(409, format!("@{name} is already in your channels."));
    }
    list.push(json!({
        "name": name,
        "enabled": true,
        "revision": 1,
        "lastPostId": Value::Null,
        "lastReadAt": Value::Null,
        "lastError": Value::Null
    }));
    ctx.store.set_setting("job_sources", &state);
    notify_jobs(ctx);
    Reply::ok(json!({ "ok": true }))
}

fn update_telegram_channel(ctx: &Ctx, name: &str) -> Reply {
    let mut state = sources_state(ctx.store.as_ref());
    let enabled = ctx.body.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let Some(list) = state.get_mut("channels").and_then(Value::as_array_mut) else {
        return Reply::error(404, "Channel not found.");
    };
    let Some(channel) = list.iter_mut().find(|c| text(c, "name").eq_ignore_ascii_case(name)) else {
        return Reply::error(404, "Channel not found.");
    };
    let rev = channel.get("revision").and_then(Value::as_i64).unwrap_or(1);
    if let Some(expected) = ctx.body.get("revision").and_then(Value::as_i64)
        && expected != rev
    {
        return Reply::error(409, "Channel settings changed elsewhere. Reload and try again.");
    }
    if let Some(obj) = channel.as_object_mut() {
        obj.insert("enabled".to_string(), json!(enabled));
        obj.insert("revision".to_string(), json!(rev + 1));
    }
    ctx.store.set_setting("job_sources", &state);
    notify_jobs(ctx);
    Reply::ok(json!({ "ok": true }))
}

fn delete_telegram_channel(ctx: &Ctx, name: &str) -> Reply {
    let mut state = sources_state(ctx.store.as_ref());
    let Some(list) = state.get_mut("channels").and_then(Value::as_array_mut) else {
        return Reply::error(404, "Channel not found.");
    };
    let before = list.len();
    list.retain(|c| !text(c, "name").eq_ignore_ascii_case(name));
    if list.len() == before {
        return Reply::error(404, "Channel not found.");
    }
    ctx.store.set_setting("job_sources", &state);
    notify_jobs(ctx);
    Reply::ok(json!({ "ok": true }))
}

fn update_source_switch(ctx: &Ctx, key: &str) -> Reply {
    let mut state = sources_state(ctx.store.as_ref());
    let enabled = ctx.body.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let Some(list) = state.get_mut("switches").and_then(Value::as_array_mut) else {
        return Reply::error(404, "Scanner switch not found.");
    };
    let Some(entry) = list.iter_mut().find(|s| text(s, "key") == key) else {
        return Reply::error(404, "Scanner switch not found.");
    };
    let rev = entry.get("revision").and_then(Value::as_i64).unwrap_or(1);
    if let Some(expected) = ctx.body.get("revision").and_then(Value::as_i64)
        && expected != rev
    {
        return Reply::error(409, "Scanner settings changed elsewhere. Reload and try again.");
    }
    if let Some(obj) = entry.as_object_mut() {
        obj.insert("enabled".to_string(), json!(enabled));
        obj.insert("revision".to_string(), json!(rev + 1));
    }
    ctx.store.set_setting("job_sources", &state);
    notify_jobs(ctx);
    Reply::ok(json!({ "ok": true }))
}

fn helper_state(store: &Store) -> Value {
    store
        .setting("job_helper")
        .unwrap_or_else(|| json!({ "helpers": [], "pairing": Value::Null }))
}

fn get_helper(ctx: &Ctx) -> Reply {
    let state = helper_state(ctx.store.as_ref());
    Reply::ok(json!({
        "helpers": state.get("helpers").cloned().unwrap_or_else(|| json!([])),
        "pairing": state
            .get("pairing")
            .and_then(|p| p.get("expiresAt").cloned())
            .map(|expires| json!({ "expiresAt": expires }))
            .unwrap_or(Value::Null),
    }))
}

fn create_helper_pairing(ctx: &Ctx) -> Reply {
    let mut state = helper_state(ctx.store.as_ref());
    let raw = uuid().replace('-', "");
    let code = format!(
        "{}-{}",
        raw[..4].to_ascii_uppercase(),
        raw[4..8].to_ascii_uppercase()
    );
    let expires_at = (chrono::Utc::now() + chrono::Duration::minutes(10))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if let Some(obj) = state.as_object_mut() {
        obj.insert(
            "pairing".to_string(),
            json!({ "code": code, "expiresAt": expires_at }),
        );
    }
    ctx.store.set_setting("job_helper", &state);
    notify_jobs(ctx);
    Reply::ok(json!({ "code": code, "expiresAt": expires_at }))
}

fn cancel_helper_pairing(ctx: &Ctx) -> Reply {
    let mut state = helper_state(ctx.store.as_ref());
    if let Some(obj) = state.as_object_mut() {
        obj.insert("pairing".to_string(), Value::Null);
    }
    ctx.store.set_setting("job_helper", &state);
    notify_jobs(ctx);
    Reply::ok(json!({ "ok": true }))
}

fn revoke_helper(ctx: &Ctx, id: &str) -> Reply {
    let mut state = helper_state(ctx.store.as_ref());
    let Some(helpers) = state.get_mut("helpers").and_then(Value::as_array_mut) else {
        return Reply::error(404, "Helper not found.");
    };
    let before = helpers.len();
    helpers.retain(|h| text(h, "id") != id);
    if helpers.len() == before {
        return Reply::error(404, "Helper not found.");
    }
    ctx.store.set_setting("job_helper", &state);
    notify_jobs(ctx);
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
    fn jobs_sources_and_companies_routes_work() {
        let (store, live, runtime) = test_env();
        let prof = call(&store, &live, &runtime, "GET", "/api/jobs/profile", Value::Null);
        assert_eq!(prof.status, 200);
        assert_eq!(prof.body["profile"]["waves"]["size"], 5);

        let added = call(
            &store,
            &live,
            &runtime,
            "POST",
            "/api/jobs/sources/telegram",
            json!({ "name": "https://t.me/s/example_jobs" }),
        );
        assert_eq!(added.status, 200);

        let sources = call(&store, &live, &runtime, "GET", "/api/jobs/sources", Value::Null);
        assert_eq!(sources.body["channels"][0]["name"], "example_jobs");

        let toggled = call(
            &store,
            &live,
            &runtime,
            "PUT",
            "/api/jobs/sources/switches/scan%3Afull",
            json!({ "enabled": true, "revision": 1 }),
        );
        assert_eq!(toggled.status, 200);

        let companies = call(&store, &live, &runtime, "GET", "/api/jobs/companies", Value::Null);
        assert_eq!(companies.status, 200);
        assert_eq!(companies.body["total"], 0);
    }

    #[test]
    fn jobs_zod_validation_rejects_invalid_profile_templates_and_cv_mime() {
        let (store, live, runtime) = test_env();

        let bad_profile = call(
            &store,
            &live,
            &runtime,
            "PATCH",
            "/api/jobs/profile",
            json!({ "profile": { "unexpectedField": true } }),
        );
        assert_eq!(bad_profile.status, 400);

        let bad_country = call(
            &store,
            &live,
            &runtime,
            "PATCH",
            "/api/jobs/profile",
            json!({ "profile": { "languageRule": { "byCountry": { "usa": "en" } } } }),
        );
        assert_eq!(bad_country.status, 400);

        let bad_limits = call(
            &store,
            &live,
            &runtime,
            "PATCH",
            "/api/jobs/profile",
            json!({ "profile": { "inviteLimits": { "daily": 150 } } }),
        );
        assert_eq!(bad_limits.status, 400);

        let bad_tpl = call(
            &store,
            &live,
            &runtime,
            "PUT",
            "/api/jobs/templates",
            json!({
                "templates": [{
                    "language": "en",
                    "body": "Hi {first_name}, check {unknown_var}",
                    "greetings": { "default": "Hi" }
                }]
            }),
        );
        assert_eq!(bad_tpl.status, 400);

        let ok_tpl = call(
            &store,
            &live,
            &runtime,
            "PUT",
            "/api/jobs/templates",
            json!({
                "templates": [{
                    "language": "en-US",
                    "body": "{greeting} {first_name} at {company}",
                    "greetings": { "default": "Hello" }
                }]
            }),
        );
        assert_eq!(ok_tpl.status, 200);

        let bad_cv = call(
            &store,
            &live,
            &runtime,
            "POST",
            "/api/jobs/cvs",
            json!({
                "label": "Malicious",
                "file": { "id": "f1", "name": "x.exe", "mimeType": "application/x-msdownload" }
            }),
        );
        assert_eq!(bad_cv.status, 400);
    }
}
