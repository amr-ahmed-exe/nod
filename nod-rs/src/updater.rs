use crate::db::data_directory;
use crate::runtime::storage;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const DEFAULT_REPO: &str = "amr-ahmed-exe/nod";

fn update_repo() -> String {
    std::env::var("NOD_UPDATE_REPO")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

pub fn release_page_url() -> String {
    format!("https://github.com/{}/releases/latest", update_repo())
}

#[derive(Clone, Debug)]
struct UpdateFailure {
    id: u64,
    phase: String,
    message: String,
    dismissed: bool,
}

#[derive(Clone, Debug)]
struct UpdateState {
    phase: String,
    checking: bool,
    progress: Option<u8>,
    ready_version: Option<String>,
    dismissed_version: Option<String>,
    failure: Option<UpdateFailure>,
    next_failure_id: u64,
    staged_path: Option<PathBuf>,
    staged_sha256: Option<String>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            phase: "idle".to_string(),
            checking: false,
            progress: None,
            ready_version: None,
            dismissed_version: None,
            failure: None,
            next_failure_id: 1,
            staged_path: None,
            staged_sha256: None,
        }
    }
}

type StatusListener = Box<dyn Fn(Value) + Send + Sync>;

pub struct UpdateManager {
    current_version: String,
    state: Mutex<UpdateState>,
    listener: Mutex<Option<StatusListener>>,
    busy: AtomicBool,
    updates_dir: PathBuf,
}

impl UpdateManager {
    pub fn new() -> Arc<Self> {
        let updates_dir = data_directory().join("updates");
        let _ = fs::create_dir_all(&updates_dir);
        Arc::new(Self {
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            state: Mutex::new(UpdateState::default()),
            listener: Mutex::new(None),
            busy: AtomicBool::new(false),
            updates_dir,
        })
    }

    pub fn set_listener(&self, cb: StatusListener) {
        if let Ok(mut guard) = self.listener.lock() {
            *guard = Some(cb);
        }
    }

    pub fn status_json(&self) -> Value {
        let guard = self.state.lock().ok();
        let st = guard.as_deref().cloned().unwrap_or_default();
        state_to_json(&st)
    }

    pub fn ready_version(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|s| (s.phase == "ready").then(|| s.ready_version.clone()).flatten())
    }

    pub fn start_background_checks(self: &Arc<Self>) {
        if std::env::var_os("NOD_DISABLE_AUTO_UPDATE").is_some() {
            return;
        }
        let mgr = Arc::clone(self);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(15));
            loop {
                mgr.run_check(false);
                thread::sleep(Duration::from_secs(6 * 3600));
            }
        });
    }

    pub fn trigger_check(self: &Arc<Self>, manual: bool) {
        if self.busy.load(Ordering::SeqCst) {
            return;
        }
        let mgr = Arc::clone(self);
        thread::spawn(move || {
            mgr.run_check(manual);
        });
    }

    pub fn dismiss(&self, version_or_id: &str) -> Value {
        if let Ok(mut st) = self.state.lock() {
            if let Ok(fid) = version_or_id.parse::<u64>()
                && let Some(ref mut f) = st.failure
                && f.id == fid
            {
                f.dismissed = true;
            } else if !version_or_id.is_empty() {
                st.dismissed_version = Some(version_or_id.to_string());
            }
        }
        let snap = self.status_json();
        self.emit(snap.clone());
        snap
    }

    pub fn is_restart_failure(&self, id: u64) -> bool {
        self.state
            .lock()
            .ok()
            .and_then(|st| st.failure.as_ref().map(|f| f.id == id && f.phase == "restart"))
            .unwrap_or(false)
    }

    pub fn retry(self: &Arc<Self>, _id: u64) -> Value {
        self.trigger_check(true);
        self.status_json()
    }

    pub fn restart_and_apply(&self) -> Result<(), String> {
        let res = self.restart_and_apply_inner();
        if let Err(ref msg) = res {
            self.set_failure("restart", msg.clone(), true);
        }
        res
    }

    fn restart_and_apply_inner(&self) -> Result<(), String> {
        let (staged, expected_sha) = {
            let st = self
                .state
                .lock()
                .map_err(|_| "Updater state unavailable.".to_string())?;
            let path = st
                .staged_path
                .clone()
                .ok_or_else(|| "No downloaded update is ready yet.".to_string())?;
            let sha = st
                .staged_sha256
                .clone()
                .ok_or_else(|| "Staged update checksum is missing.".to_string())?;
            (path, sha)
        };
        if !staged.is_file() {
            return Err("The downloaded update file could not be found.".to_string());
        }
        let actual = sha256_file(&staged)?;
        if actual != expected_sha {
            return Err("The staged update failed its SHA-256 verification check.".to_string());
        }
        let current_exe =
            std::env::current_exe().map_err(|e| format!("Could not resolve current executable: {e}"))?;
        spawn_self_replace(&current_exe, &staged)
    }

    fn emit(&self, payload: Value) {
        if let Ok(guard) = self.listener.lock()
            && let Some(cb) = guard.as_ref()
        {
            cb(payload);
        }
    }

    fn set_phase(&self, phase: &str, checking: bool, progress: Option<u8>) {
        let snap = {
            let Ok(mut st) = self.state.lock() else {
                return;
            };
            st.phase = phase.to_string();
            st.checking = checking;
            st.progress = progress;
            if checking {
                st.failure = None;
            }
            state_to_json(&st)
        };
        self.emit(snap);
    }

    fn set_failure(&self, phase: &str, message: String, report: bool) {
        let snap = {
            let Ok(mut st) = self.state.lock() else {
                return;
            };
            st.phase = "idle".to_string();
            st.checking = false;
            st.progress = None;
            if report {
                let id = st.next_failure_id;
                st.next_failure_id += 1;
                st.failure = Some(UpdateFailure {
                    id,
                    phase: phase.to_string(),
                    message,
                    dismissed: false,
                });
            }
            state_to_json(&st)
        };
        self.emit(snap);
    }

    fn run_check(&self, manual: bool) {
        if self
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let res = self.perform_update_check();
        self.busy.store(false, Ordering::SeqCst);
        if let Err((phase, msg)) = res {
            self.set_failure(phase, msg, manual);
        }
    }

    fn perform_update_check(&self) -> Result<(), (&'static str, String)> {
        self.set_phase("checking", true, None);
        let repo = update_repo();
        let api_url = std::env::var("NOD_UPDATE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| format!("https://api.github.com/repos/{repo}/releases/latest"));

        if !is_safe_release_url(&api_url) {
            return Err(("checking", "Invalid update feed URL.".to_string()));
        }

        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(12))
            .timeout_read(Duration::from_secs(30))
            .user_agent("Nod-Updater/1.0")
            .build();
        let resp = agent
            .get(&api_url)
            .set("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| ("checking", format!("Could not check for updates: {e}")))?;
        let release: Value = serde_json::from_reader(resp.into_reader())
            .map_err(|e| ("checking", format!("Invalid update feed response: {e}")))?;

        if release.get("draft").and_then(Value::as_bool) == Some(true) {
            self.set_phase("idle", false, None);
            return Ok(());
        }

        let tag = release
            .get("tag_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let remote_ver = normalize_version(tag);
        if remote_ver.is_empty() || !is_newer_version(&remote_ver, &self.current_version) {
            self.set_phase("idle", false, None);
            return Ok(());
        }

        if let Some(existing_ready) = self.ready_version()
            && existing_ready == remote_ver
        {
            self.set_phase("ready", false, Some(100));
            return Ok(());
        }

        let assets = release
            .get("assets")
            .and_then(Value::as_array)
            .ok_or(("preparing", "Release assets are missing.".to_string()))?;
        let target_asset_name = platform_asset_name();
        let asset = assets
            .iter()
            .find(|a| a.get("name").and_then(Value::as_str) == Some(target_asset_name))
            .ok_or((
                "preparing",
                format!("Release {remote_ver} does not include {target_asset_name}."),
            ))?;
        let dl_url = asset
            .get("browser_download_url")
            .and_then(Value::as_str)
            .ok_or(("preparing", "Update asset URL missing.".to_string()))?;
        if !is_safe_release_url(dl_url) {
            return Err(("preparing", "Update download URL must use HTTPS.".to_string()));
        }
        let expected_size = asset.get("size").and_then(Value::as_u64).unwrap_or(0);

        let expected_sha = resolve_asset_sha256(asset, assets, target_asset_name, &agent)
            .map_err(|e| ("preparing", e))?;

        let staged = self.updates_dir.join(target_asset_name);
        if staged.is_file()
            && let Ok(existing_hash) = sha256_file(&staged)
            && existing_hash == expected_sha
        {
            self.mark_ready(remote_ver, staged, expected_sha);
            return Ok(());
        }

        self.set_phase("downloading", true, Some(0));
        let part_path = self.updates_dir.join(format!("{target_asset_name}.part"));
        let dl_resp = agent
            .get(dl_url)
            .call()
            .map_err(|e| ("downloading", format!("Update download failed: {e}")))?;

        let total = expected_size.max(
            dl_resp
                .header("Content-Length")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0),
        );
        let mut reader = dl_resp.into_reader();
        let mut file = File::create(&part_path)
            .map_err(|e| ("downloading", format!("Could not write staged update: {e}")))?;
        let mut buf = [0u8; 32 * 1024];
        let mut hasher = Sha256::new();
        let mut received: u64 = 0;
        let mut last_pct: u8 = 0;
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| ("downloading", format!("Interrupted while downloading update: {e}")))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])
                .map_err(|e| ("downloading", format!("Disk write error during update: {e}")))?;
            hasher.update(&buf[..n]);
            received += n as u64;
            if let Some(raw_pct) = received.saturating_mul(100).checked_div(total) {
                let pct = raw_pct.min(99) as u8;
                if pct >= last_pct.saturating_add(5) {
                    last_pct = pct;
                    self.set_phase("downloading", true, Some(pct));
                }
            }
        }
        file.flush()
            .map_err(|e| ("downloading", format!("Could not finalize update file: {e}")))?;
        drop(file);

        let actual_sha: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if actual_sha != expected_sha {
            let _ = fs::remove_file(&part_path);
            return Err((
                "downloading",
                "Downloaded update failed its SHA-256 verification check.".to_string(),
            ));
        }

        storage::replace_file(&part_path, &staged)
            .map_err(|e| ("downloading", format!("Could not stage verified update: {e}")))?;

        self.mark_ready(remote_ver, staged, expected_sha);
        Ok(())
    }

    fn mark_ready(&self, version: String, staged: PathBuf, sha: String) {
        let snap = {
            let Ok(mut st) = self.state.lock() else {
                return;
            };
            st.phase = "ready".to_string();
            st.checking = false;
            st.progress = Some(100);
            st.ready_version = Some(version);
            st.failure = None;
            st.staged_path = Some(staged);
            st.staged_sha256 = Some(sha);
            state_to_json(&st)
        };
        self.emit(snap);
    }
}

fn state_to_json(st: &UpdateState) -> Value {
    let failure_val = st.failure.as_ref().map(|f| {
        json!({
            "id": f.id,
            "phase": f.phase,
            "message": f.message,
            "dismissed": f.dismissed,
        })
    });
    json!({
        "phase": st.phase,
        "checking": st.checking,
        "progress": st.progress,
        "readyVersion": st.ready_version,
        "dismissedVersion": st.dismissed_version,
        "failure": failure_val,
    })
}

fn platform_asset_name() -> &'static str {
    if cfg!(windows) {
        "nod-windows-x86_64.exe"
    } else if cfg!(target_arch = "aarch64") {
        "nod-linux-aarch64"
    } else {
        "nod-linux-x86_64"
    }
}

fn is_safe_release_url(raw: &str) -> bool {
    raw.starts_with("https://")
        || (cfg!(test) && raw.starts_with("http://127.0.0.1:"))
        || std::env::var_os("NOD_UPDATE_URL")
            .map(|_| raw.starts_with("http://127.0.0.1:"))
            .unwrap_or(false)
}

fn resolve_asset_sha256(
    asset: &Value,
    assets: &[Value],
    asset_name: &str,
    agent: &ureq::Agent,
) -> Result<String, String> {
    if let Some(digest) = asset
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|d| d.strip_prefix("sha256:"))
    {
        let clean = digest.trim().to_ascii_lowercase();
        if clean.len() == 64 && clean.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(clean);
        }
    }
    if let Some(sums_asset) = assets
        .iter()
        .find(|a| a.get("name").and_then(Value::as_str) == Some("SHA256SUMS.txt"))
        && let Some(sums_url) = sums_asset.get("browser_download_url").and_then(Value::as_str)
        && is_safe_release_url(sums_url)
    {
        let text = agent
            .get(sums_url)
            .call()
            .map_err(|e| format!("Could not fetch SHA256SUMS.txt: {e}"))?
            .into_string()
            .map_err(|e| format!("Could not read SHA256SUMS.txt: {e}"))?;
        for line in text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let fname = parts[1].trim_start_matches('*');
                if fname == asset_name
                    && parts[0].len() == 64
                    && parts[0].bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Ok(parts[0].to_ascii_lowercase());
                }
            }
        }
    }
    Err(format!("Missing verified SHA-256 checksum for {asset_name}."))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = File::open(path).map_err(|e| format!("Could not open update file: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 32 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("Could not hash file: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

pub fn normalize_version(tag: &str) -> String {
    let t = tag.trim();
    let t = t.strip_prefix("nod-v").unwrap_or(t);
    let t = t.strip_prefix('v').unwrap_or(t);
    t.trim().to_string()
}

pub fn is_newer_version(candidate: &str, current: &str) -> bool {
    let parse = |s: &str| -> Option<(u64, u64, u64)> {
        let core = s.split(['-', '+']).next().unwrap_or("");
        let mut it = core.split('.');
        let maj = it.next()?.parse().ok()?;
        let min = it.next().unwrap_or("0").parse().ok()?;
        let pat = it.next().unwrap_or("0").parse().ok()?;
        Some((maj, min, pat))
    };
    match (parse(candidate), parse(current)) {
        (Some(cand), Some(cur)) => cand > cur,
        _ => false,
    }
}

fn spawn_self_replace(current_exe: &Path, staged: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use crate::runtime::windows;
        let pid = std::process::id();
        let script = format!(
            "$p={pid}; while (Get-Process -Id $p -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 200 }}; \
            Copy-Item -LiteralPath $env:NOD_UPD_SRC -Destination $env:NOD_UPD_DST -Force; \
            Remove-Item -LiteralPath $env:NOD_UPD_SRC -Force -ErrorAction SilentlyContinue; \
            Start-Process -FilePath $env:NOD_UPD_DST"
        );
        let mut cmd = std::process::Command::new(windows::powershell_exe());
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ]);
        cmd.env("NOD_UPD_SRC", staged);
        cmd.env("NOD_UPD_DST", current_exe);
        windows::hide_console(&mut cmd);
        cmd.spawn()
            .map_err(|e| format!("Could not launch updater swapper: {e}"))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(staged, perms)
            .map_err(|e| format!("Could not mark update executable: {e}"))?;
        let _ = fs::rename(staged, current_exe).or_else(|_| fs::copy(staged, current_exe).map(|_| ()));
        std::process::Command::new(current_exe)
            .spawn()
            .map_err(|e| format!("Could not restart Nod: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_http::{Header, Response, Server, StatusCode};

    #[test]
    fn semver_parsing_compares_versions_accurately() {
        assert_eq!(normalize_version("nod-v0.10.2"), "0.10.2");
        assert_eq!(normalize_version("v1.0.0"), "1.0.0");
        assert!(is_newer_version("0.2.0", "0.1.0"));
        assert!(is_newer_version("0.10.0", "0.9.6"));
        assert!(!is_newer_version("0.1.0", "0.1.0"));
        assert!(!is_newer_version("0.0.9", "0.1.0"));
    }

    #[test]
    fn updater_downloads_and_verifies_sha256_before_marking_ready() {
        let binary_bytes = b"fake-nod-binary-payload-v0.99.0".to_vec();
        let expected_sha: String = Sha256::digest(&binary_bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let asset_name = platform_asset_name().to_string();

        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let srv_asset = asset_name.clone();
        let srv_sha = expected_sha.clone();
        let srv_bytes = binary_bytes.clone();

        let handle = thread::spawn(move || {
            for _ in 0..3 {
                let Ok(req) = server.recv() else { break };
                let path = req.url().to_string();
                if path == "/latest" {
                    let body = json!({
                        "tag_name": "v0.99.0",
                        "draft": false,
                        "assets": [
                            {
                                "name": srv_asset,
                                "size": srv_bytes.len(),
                                "browser_download_url": format!("http://127.0.0.1:{port}/download/{srv_asset}")
                            },
                            {
                                "name": "SHA256SUMS.txt",
                                "size": 128,
                                "browser_download_url": format!("http://127.0.0.1:{port}/SHA256SUMS.txt")
                            }
                        ]
                    })
                    .to_string();
                    let mut r = Response::from_string(body).with_status_code(StatusCode(200));
                    if let Ok(h) = Header::from_bytes("Content-Type", "application/json") {
                        r.add_header(h);
                    }
                    let _ = req.respond(r);
                } else if path == "/SHA256SUMS.txt" {
                    let sums = format!("{srv_sha}  {srv_asset}\n");
                    let _ = req.respond(Response::from_string(sums).with_status_code(StatusCode(200)));
                } else {
                    let _ = req.respond(Response::from_data(srv_bytes.clone()).with_status_code(StatusCode(200)));
                }
            }
        });

        let temp_dir = std::env::temp_dir().join(format!("nod-upd-test-{}", storage::random_suffix(8)));
        let updates_dir = temp_dir.join("updates");
        let _ = fs::create_dir_all(&updates_dir);
        let mgr = Arc::new(UpdateManager {
            current_version: "0.1.0".to_string(),
            state: Mutex::new(UpdateState::default()),
            listener: Mutex::new(None),
            busy: AtomicBool::new(false),
            updates_dir,
        });

        unsafe {
            std::env::set_var("NOD_UPDATE_URL", format!("http://127.0.0.1:{port}/latest"));
        }
        mgr.run_check(true);
        unsafe {
            std::env::remove_var("NOD_UPDATE_URL");
        }

        let snap = mgr.status_json();
        assert_eq!(snap["phase"], json!("ready"));
        assert_eq!(snap["readyVersion"], json!("0.99.0"));
        assert_eq!(mgr.ready_version().as_deref(), Some("0.99.0"));
        handle.join().unwrap();
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
