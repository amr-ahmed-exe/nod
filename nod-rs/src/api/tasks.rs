use super::{Ctx, Reply};
use crate::db::Store;
use serde_json::{json, Value};

pub fn handle(ctx: &Ctx) -> Option<Reply> {
    let method = ctx.method;
    let path = ctx.path;

    if method == "GET" && path == "/api/tasks" {
        return Some(list(ctx));
    }
    if let Some(rest) = path.strip_prefix("/api/tasks/") {
        return Some(by_id(ctx, rest));
    }
    None
}

fn list(ctx: &Ctx) -> Reply {
    let tasks = ctx.store.all("tasks");
    let views: Vec<Value> = tasks
        .iter()
        .filter(|t| !t.get("archived").and_then(Value::as_bool).unwrap_or(false))
        .map(task_view)
        .collect();
    Reply::ok(Value::Array(views))
}

pub fn thread_tasks(store: &Store, thread: &str) -> Vec<Value> {
    store
        .all("tasks")
        .into_iter()
        .filter(|task| task.get("threadId").and_then(Value::as_str) == Some(thread))
        .map(|t| task_view(&t))
        .collect()
}

fn by_id(ctx: &Ctx, rest: &str) -> Reply {
    let mut parts = rest.split('/');
    let id = parts.next().unwrap_or("");
    let action = parts.next();

    match (ctx.method, action) {
        ("GET", None) => match ctx.store.get("tasks", id) {
            Some(task) => Reply::ok(task_view(&task)),
            None => Reply::error(404, "Task not found."),
        },
        ("PATCH", None) => match ctx.store.patch("tasks", id, &ctx.body) {
            Some(task) => {
                ctx.refresh();
                Reply::ok(task_view(&task))
            }
            None => Reply::error(404, "Task not found."),
        },
        ("DELETE", None) => {
            ctx.store.delete("tasks", id);
            ctx.refresh();
            Reply::ok(json!({ "ok": true }))
        }
        ("POST", Some("stop")) => {
            if let Some(mut task) = ctx.store.get("tasks", id) {
                if let Some(obj) = task.as_object_mut() {
                    obj.insert("status".to_string(), json!("stopped"));
                }
                let _ = ctx.store.put("tasks", id, &task);
            }
            ctx.refresh();
            Reply::ok(json!({ "ok": true }))
        }
        _ => Reply::error(404, "Task not found."),
    }
}

fn task_view(task: &Value) -> Value {
    task.clone()
}
