//! Private computer lifecycle manager orchestrating WSL2, assets, guest Docker, and health.

pub mod assets;
pub mod errors;
pub mod health;
pub mod state;
pub mod storage;
pub mod windows;
pub mod wsl;

use crate::runtime::assets::RuntimeManifest;
use crate::runtime::errors::{
    is_missing_wsl_exit, is_unrepairable_wsl_code, parse_wsl_code, Redactor, RuntimeError,
    NOD_COMPUTER_NETWORK_INVALID, NOD_WSL_COMMAND_FAILED,
};
use crate::runtime::health::{
    browser_container, browser_network, control_network, postgres_container, workspace_container,
    workspace_network, DockerTransport, ServiceHealth,
};
use crate::runtime::state::{PersistedState, Phase, SetupPlan, Status};
use crate::runtime::storage::RuntimePaths;
use crate::runtime::wsl::Wsl;
use crate::server::Live;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::Duration;

const LABEL_MANAGED_BY: &str = "io.nod.managed=true";
const STOP_TIMEOUT_SECS: &str = "20";

/// Network isolation script applied inside the WSL guest before starting workloads.
const PROTECT_NETWORKS_SCRIPT: &str = r#"set -eu
ensure_chain() {
  local table="$1"
  local chain="$2"
  iptables -t "$table" -nL "$chain" >/dev/null 2>&1 || iptables -t "$table" -N "$chain"
  iptables -t "$table" -F "$chain"
}
ensure_jump() {
  local table="$1"
  local parent="$2"
  local target="$3"
  if ! iptables -t "$table" -C "$parent" -j "$target" >/dev/null 2>&1; then
    iptables -t "$table" -I "$parent" 1 -j "$target"
  fi
}
ensure_chain filter NOD-WORKLOADS
for cidr in 169.254.0.0/16 10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 100.64.0.0/10; do
  iptables -A NOD-WORKLOADS -i nod-workspace -d "$cidr" -j REJECT
  iptables -A NOD-WORKLOADS -i nod-browser -d "$cidr" -j REJECT
done
iptables -A NOD-WORKLOADS -o nod-control -m conntrack --ctstate NEW -j REJECT
iptables -A NOD-WORKLOADS -j RETURN
ensure_jump filter DOCKER-USER NOD-WORKLOADS
if ! iptables -C INPUT -i nod-workspace -j DROP >/dev/null 2>&1; then
  iptables -I INPUT 1 -i nod-workspace -j DROP
fi
if ! iptables -C INPUT -i nod-browser -j DROP >/dev/null 2>&1; then
  iptables -I INPUT 1 -i nod-browser -j DROP
fi
"#;

type BridgeCallback = Box<dyn Fn(&str, Value) + Send + Sync>;

/// Thread-safe owner of Nod's Windows Private Computer runtime lifecycle.
pub struct RuntimeManager {
    paths: RwLock<RuntimePaths>,
    status: Mutex<Status>,
    setup_gate: (Mutex<bool>, Condvar),
    cancelled: AtomicBool,
    holder: Mutex<Option<Child>>,
    persisted: Mutex<Option<PersistedState>>,
    latest_health: Mutex<Option<ServiceHealth>>,
    live: Arc<Live>,
    bridge: Mutex<Option<BridgeCallback>>,
}

impl RuntimeManager {
    /// Creates a new `RuntimeManager` starting cleanly in `Idle` without guessing readiness.
    pub fn new(live: Arc<Live>) -> Arc<Self> {
        let mut paths = RuntimePaths::discover();
        let saved_settings = storage::read_json(&paths.settings_path);
        if let Some(custom_dir) = saved_settings
            .as_ref()
            .and_then(|v| v.get("computerDirectory"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            paths.computer_root = PathBuf::from(custom_dir).join(&paths.distribution);
        }
        let saved_state = read_persisted_state(&paths.state_path);
        let dist = Some(paths.distribution.clone());
        let initial = Status::idle("Private computer is idle.", dist);
        let manager = Arc::new(Self {
            paths: RwLock::new(paths),
            status: Mutex::new(initial),
            setup_gate: (Mutex::new(false), Condvar::new()),
            cancelled: AtomicBool::new(false),
            holder: Mutex::new(None),
            persisted: Mutex::new(saved_state),
            latest_health: Mutex::new(None),
            live,
            bridge: Mutex::new(None),
        });
        manager.log_line("Nod runtime manager initialized.");
        manager
    }

    /// Registers the Wry WebView push notifier used for `onStatus` and `onPreferences`.
    pub fn set_bridge_notifier(&self, cb: BridgeCallback) {
        if let Ok(mut guard) = self.bridge.lock() {
            *guard = Some(cb);
        }
    }

    #[cfg(test)]
    pub(crate) fn force_ready_for_test(&self) {
        if let Ok(mut guard) = self.latest_health.lock() {
            *guard = Some(ServiceHealth {
                docker_ready: true,
                postgres_ready: true,
                postgres_port: Some(5432),
                workspace_ready: true,
                browser_ready: true,
                checked_at: storage::now_iso(),
                detail: None,
            });
        }
        self.transition_status(Phase::Ready, "Private computer is ready.", None);
    }

    /// Returns a cloned snapshot of the current runtime status.
    pub fn status(&self) -> Status {
        self.status
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// Returns the JSON status payload consumed by both `setup.html` and the main React app.
    pub fn status_json(&self) -> Value {
        self.status().to_json()
    }

    /// Returns true strictly when all startup stages and service health probes have succeeded.
    pub fn is_ready(&self) -> bool {
        self.status().state == Phase::Ready
    }

    /// Returns true only when the runtime is ready and live browser health is passing.
    pub fn browser_ready(&self) -> bool {
        if !self.is_ready() {
            return false;
        }
        if let Some(report) = self
            .latest_health
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            && report.all_ready()
                && let Some(health) = self.probe_live_health() {
                    return health.browser_ready;
                }
        false
    }

    /// Returns a live health probe when the runtime has state and transport present.
    pub fn probe_live_health(&self) -> Option<ServiceHealth> {
        let paths = self.paths.read().ok()?.clone();
        let manifest = assets::find_manifest(&paths).ok()??;
        let state = self.persisted.lock().ok()?.clone()?;
        let port = state.port?;
        let identity = storage::identity_for(&paths.distribution);
        let docker = DockerTransport::for_manifest(
            &manifest,
            paths.distribution.clone(),
            paths.control_root.join("docker-config"),
            Some(state.password),
        );
        let report = health::probe_all(&docker, &identity, Some(port));
        if let Ok(mut guard) = self.latest_health.lock() {
            *guard = Some(report.clone());
        }
        Some(report)
    }

    /// Runs fixed-argument commands inside the hardened private browser container when ready.
    pub fn exec_browser_command(
        &self,
        args: &[&str],
        input: Option<&str>,
        timeout: Duration,
    ) -> Result<wsl::CommandOutput, RuntimeError> {
        let redactor = self.redactor();
        if !self.is_ready() {
            return Err(RuntimeError::not_ready(
                "The private browser is not ready yet.",
                &redactor,
            ));
        }
        let paths = self
            .paths
            .read()
            .map_err(|_| RuntimeError::not_ready("Runtime paths unavailable.", &redactor))?
            .clone();
        let manifest = assets::find_manifest(&paths)?
            .ok_or_else(|| RuntimeError::archive_required("The runtime manifest is missing."))?;
        let identity = storage::identity_for(&paths.distribution);
        let container = browser_container(&identity);
        let docker = DockerTransport::for_manifest(
            &manifest,
            paths.distribution.clone(),
            paths.control_root.join("docker-config"),
            None,
        );
        if let Err(err) = health::check_browser(&docker, &identity) {
            return Err(RuntimeError::not_ready(err, &redactor));
        }
        let mut cmd_args = Vec::with_capacity(args.len() + 3);
        cmd_args.push("exec");
        if input.is_some() {
            cmd_args.push("-i");
        }
        cmd_args.push(&container);
        cmd_args.extend_from_slice(args);
        docker
            .run_with_input(&cmd_args, input, timeout, false)
            .map_err(|err| RuntimeError::command_failed("browser", err, "", None, &redactor))
    }

    /// Starts background warmup when runtime is still `Idle`.
    pub fn warmup(self: &Arc<Self>) {
        if self.status().state != Phase::Idle {
            return;
        }
        let me = Arc::clone(self);
        thread::spawn(move || {
            let _ = me.prepare(false);
        });
    }

    /// Runs single-flight setup & startup, joining any in-progress setup run safely.
    pub fn prepare(&self, retry: bool) -> Status {
        let (lock, cvar) = &self.setup_gate;
        let mut active = match lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if *active {
            while *active {
                active = match cvar.wait(active) {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
            return self.status();
        }
        let current = self.status();
        if !retry && current.state == Phase::Ready {
            return current;
        }
        if !retry && matches!(current.state, Phase::NeedsWsl | Phase::Error) {
            return current;
        }
        *active = true;
        drop(active);

        self.cancelled.store(false, Ordering::SeqCst);
        let result = self.run_prepare_sequence();
        if let Err(failure) = result {
            self.fail_status(&failure);
        }

        if let Ok(mut guard) = lock.lock() {
            *guard = false;
            cvar.notify_all();
        }
        self.status()
    }

    /// Requests elevated Windows WSL2 installation and reports restart requirement cleanly.
    pub fn install_wsl(&self) -> Status {
        let redactor = self.redactor();
        self.transition_status(Phase::CheckingWsl, "Installing Windows Subsystem for Linux…", Some(0.05));
        match wsl::install_wsl() {
            Ok(()) => {
                let err = RuntimeError::new(
                    errors::NOD_WSL_REQUIRED,
                    "Windows Subsystem for Linux was installed. Restart Windows, then open Nod again.",
                )
                .step("wsl")
                .needs_wsl()
                .restart_required(true);
                self.fail_status(&err);
            }
            Err(msg) => {
                let mut err = RuntimeError::wsl_broken(None, msg, &redactor);
                err = err.restart_required(true);
                self.fail_status(&err);
            }
        }
        self.status()
    }

    /// Opens native Windows file picker (when `archive` is None), validates/installs archive, and prepares runtime.
    pub fn import_runtime(&self, archive: Option<PathBuf>) -> Status {
        let redactor = self.redactor();
        let chosen = match archive {
            Some(path) => Some(path),
            None => match windows::pick_runtime_archive(None) {
                Ok(path) => path,
                Err(err) => {
                    self.fail_status(&RuntimeError::archive_required(err));
                    return self.status();
                }
            },
        };
        let Some(archive_path) = chosen else {
            return self.status();
        };
        self.transition_status(Phase::VerifyingRuntime, "Checking the selected runtime archive…", Some(0.35));
        let paths = match self.paths.read() {
            Ok(guard) => guard.clone(),
            Err(_) => return self.status(),
        };
        match assets::install_runtime_archive(&archive_path, &paths, &redactor) {
            Ok(_) => {
                self.log_line("Imported user-selected Windows runtime archive.");
                self.prepare(true)
            }
            Err(failure) => {
                self.fail_status(&failure);
                self.status()
            }
        }
    }

    /// Lets the user select an alternate storage folder for the WSL virtual disk without erasing data.
    pub fn choose_computer_directory(&self, dir: Option<PathBuf>) -> Status {
        let paths_snapshot = match self.paths.read() {
            Ok(guard) => guard.clone(),
            Err(_) => return self.status(),
        };
        let chosen = match dir {
            Some(path) => Some(path),
            None => match windows::pick_folder(
                "Choose a folder with enough free space for Nod's private computer",
                Some(&paths_snapshot.runtime_root),
            ) {
                Ok(path) => path,
                Err(err) => {
                    self.fail_status(&RuntimeError::new(
                        errors::NOD_COMPUTER_STORAGE_FULL,
                        err,
                    ));
                    return self.status();
                }
            },
        };
        let Some(selected) = chosen else {
            return self.status();
        };
        let mut settings = self.get_settings();
        if let Some(obj) = settings.as_object_mut() {
            obj.insert(
                "computerDirectory".to_string(),
                Value::String(selected.to_string_lossy().to_string()),
            );
        }
        let _ = self.save_settings(settings);
        if let Ok(mut guard) = self.paths.write() {
            guard.computer_root = selected.join(&guard.distribution);
        }
        self.log_line("Updated private computer storage directory.");
        self.prepare(true)
    }

    /// Gracefully stops guest containers, terminates the WSL distro, and kills the holder process.
    pub fn stop(&self) -> Result<Status, RuntimeError> {
        self.cancelled.store(true, Ordering::SeqCst);
        let redactor = self.redactor();
        self.transition_status(Phase::Stopping, "Stopping private computer…", None);
        let paths = self
            .paths
            .read()
            .map_err(|_| RuntimeError::stop_failed("Runtime paths locked.", &redactor))?
            .clone();
        let identity = storage::identity_for(&paths.distribution);

        let docker = if let Ok(Some(manifest)) = assets::find_manifest(&paths) {
            Some(DockerTransport::for_manifest(
                &manifest,
                paths.distribution.clone(),
                paths.control_root.join("docker-config"),
                None,
            ))
        } else {
            let wsl_bin = windows::wsl_exe();
            windows::is_trusted_windows_binary(&wsl_bin).then(|| DockerTransport {
                exe: wsl_bin,
                distribution: paths.distribution.clone(),
                docker_config: paths.control_root.join("docker-config"),
                postgres_password: None,
            })
        };
        if let Some(docker) = docker {
            for container in [
                browser_container(&identity),
                workspace_container(&identity),
                postgres_container(&identity),
            ] {
                let _ = docker.run(&["stop", "--time", STOP_TIMEOUT_SECS, &container], Duration::from_secs(30), false);
            }
        }

        if let Ok(wsl) = Wsl::discover() {
            let _ = wsl.terminate(&paths.distribution);
            if let Ok(mut holder) = self.holder.lock()
                && let Some(mut child) = holder.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            if let Ok(running) = wsl.list_running(&redactor)
                && running.contains(&paths.distribution) {
                    let err = RuntimeError::stop_failed(
                        "The WSL distribution remained running after stop.",
                        &redactor,
                    );
                    self.fail_status(&err);
                    return Err(err);
                }
        }
        if let Ok(mut guard) = self.latest_health.lock() {
            *guard = None;
        }
        self.transition_status(Phase::Idle, "Private computer stopped.", None);
        self.log_line("Private computer stopped cleanly.");
        Ok(self.status())
    }

    /// Returns the startup log file path.
    pub fn startup_log_path(&self) -> PathBuf {
        self.paths
            .read()
            .map(|p| p.startup_log_path.clone())
            .unwrap_or_else(|_| RuntimePaths::discover().startup_log_path)
    }

    /// Reads desktop `settings.json` with v2 defaults matching legacy `settings.mjs`.
    pub fn get_settings(&self) -> Value {
        let paths = self.paths_snapshot();
        let existing = storage::read_json(&paths.settings_path).unwrap_or_else(|| json!({}));
        let prefs = self.get_preferences();
        let preference_step = match existing.get("onboardingPreferenceStep") {
            Some(Value::String(s)) if s == "complete" || s == "launchAtLogin" => s.as_str(),
            Some(Value::Bool(true)) => "complete",
            _ => "launchAtLogin",
        };
        json!({
            "version": 2,
            "mode": "local",
            "computerDirectory": existing.get("computerDirectory").cloned().unwrap_or(Value::Null),
            "onboardingPreferenceStep": preference_step,
            "welcome": existing.get("welcome").cloned().unwrap_or_else(|| json!({ "seen": false })),
            "theme": prefs.get("theme").cloned().unwrap_or_else(|| json!("system")),
        })
    }

    /// Merges and atomically persists desktop `settings.json`.
    pub fn save_settings(&self, patch: Value) -> Result<Value, String> {
        let paths = self.paths_snapshot();
        let mut current = storage::read_json(&paths.settings_path)
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        if let Some(obj) = patch.as_object() {
            for (k, v) in obj {
                current.insert(k.clone(), v.clone());
            }
        }
        current.insert("version".to_string(), json!(2));
        storage::write_json(&paths.settings_path, &Value::Object(current))
            .map_err(|err| err.to_string())?;
        Ok(self.get_settings())
    }

    /// Reads desktop `preferences.json` with theme/desktop defaults.
    pub fn get_preferences(&self) -> Value {
        let paths = self.paths_snapshot();
        let existing = storage::read_json(&paths.preferences_path).unwrap_or_else(|| json!({}));
        let theme = existing
            .get("theme")
            .and_then(Value::as_str)
            .filter(|t| matches!(*t, "system" | "light" | "dark" | "amoled" | "catppuccin-mocha"))
            .unwrap_or("system");
        let resolved_theme = match theme {
            "light" => "light",
            "amoled" => "amoled",
            "catppuccin-mocha" => "catppuccin-mocha",
            _ => "dark",
        };
        json!({
            "theme": theme,
            "resolvedTheme": resolved_theme,
            "launchAtLogin": existing.get("launchAtLogin").and_then(Value::as_bool).unwrap_or(false),
            "notifications": existing.get("notifications").and_then(Value::as_bool).unwrap_or(true),
        })
    }

    /// Merges, persists, and pushes updated `preferences.json` to listeners.
    pub fn save_preferences(&self, patch: Value) -> Result<Value, String> {
        let paths = self.paths_snapshot();
        let mut current = storage::read_json(&paths.preferences_path)
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        if let Some(obj) = patch.as_object() {
            for (k, v) in obj {
                if k == "theme"
                    && let Some(t) = v.as_str()
                        && !matches!(t, "system" | "light" | "dark" | "amoled" | "catppuccin-mocha") {
                            return Err("Unsupported theme preference.".to_string());
                        }
                current.insert(k.clone(), v.clone());
            }
        }
        if let Some(enabled) = patch.get("launchAtLogin").and_then(Value::as_bool) {
            windows::set_launch_at_login(enabled)?;
        }
        storage::write_json(&paths.preferences_path, &Value::Object(current))
            .map_err(|err| err.to_string())?;
        let prefs = self.get_preferences();
        self.push_bridge("nod:preferences", prefs.clone());
        Ok(prefs)
    }

    /// Reads chat read cursor for `(owner_id, thread_id)`.
    pub fn get_chat_read_cursor(&self, owner_id: &str, thread_id: &str) -> Value {
        let paths = self.paths_snapshot();
        let all = storage::read_json(&paths.chat_read_cursors_path).unwrap_or_else(|| json!({}));
        all.get(owner_id)
            .and_then(|owner| owner.get(thread_id))
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Persists chat read cursor for `(owner_id, thread_id)`.
    pub fn set_chat_read_cursor(&self, owner_id: &str, thread_id: &str, cursor: Value) -> Result<(), String> {
        let paths = self.paths_snapshot();
        let mut all = storage::read_json(&paths.chat_read_cursors_path)
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let owner_entry = all
            .entry(owner_id.to_string())
            .or_insert_with(|| json!({}))
            .as_object_mut();
        if let Some(map) = owner_entry {
            map.insert(thread_id.to_string(), cursor);
        }
        storage::write_json(&paths.chat_read_cursors_path, &Value::Object(all))
            .map_err(|err| err.to_string())
    }

    fn run_prepare_sequence(&self) -> Result<(), RuntimeError> {
        let paths = self.paths_snapshot();
        let redactor = self.redactor();

        self.transition_status(
            Phase::CheckingWsl,
            "Checking Windows Subsystem for Linux…",
            Phase::CheckingWsl.progress_start(),
        );
        let wsl = Wsl::discover()?;
        wsl.probe(&redactor)?;
        let dists = wsl.distributions(&redactor)?;
        let registered = dists.registered.contains(&paths.distribution);
        let running = dists.running.contains(&paths.distribution);

        let saved_state = read_persisted_state(&paths.state_path);
        let effective_saved = match saved_state.as_ref() {
            Some(s) if !registered && !s.initializing && !has_existing_vhdx(&paths, s) => {
                self.log_line("Saved runtime state had no WSL distribution and no virtual disk; preparing clean reinstall.");
                None
            }
            _ => saved_state.as_ref(),
        };
        let mut plan = state::classify(
            effective_saved,
            registered,
            running,
            &paths.distribution,
        )?;
        let saved_state = if effective_saved.is_none() { None } else { saved_state };
        self.log_line(&format!("Setup classified as {plan:?} (registered={registered}, running={running})."));

        self.transition_status(
            Phase::PreparingStorage,
            "Preparing private computer storage…",
            Phase::PreparingStorage.progress_start(),
        );
        paths.ensure_roots().map_err(|err| {
            RuntimeError::new(errors::NOD_COMPUTER_STORAGE_FULL, format!("Could not prepare runtime directory: {err}"))
                .step("storage")
        })?;
        fs::create_dir_all(&paths.computer_root).map_err(|err| {
            RuntimeError::new(errors::NOD_COMPUTER_STORAGE_FULL, format!("Could not prepare VM directory: {err}"))
                .step("storage")
        })?;
        if let Err(err) = windows::protect_profile_directory(&[&paths.control_root, &paths.computer_root]) {
            self.log_line(&format!("Storage ACL warning: {}", redactor.message(&err)));
        }

        let mut registered = registered;
        if plan == SetupPlan::ResumeImport && registered {
            let guest_ok = wsl
                .guest(
                    &paths.distribution,
                    &["test", "-f", "/usr/local/lib/nod/windows-guest.py"],
                    None,
                    Duration::from_secs(15),
                )
                .map(|out| out.success())
                .unwrap_or(false);
            if !guest_ok {
                self.log_line("Interrupted WSL import is incomplete; unregistering partial distribution before retrying.");
                wsl.unregister(&paths.distribution).map_err(|err| {
                    RuntimeError::import_failed(err, &redactor)
                })?;
                registered = false;
                plan = SetupPlan::Fresh;
            }
        }

        let found_manifest = match assets::find_manifest(&paths) {
            Ok(found) => found,
            Err(err) => {
                self.log_line(&format!(
                    "Ignoring unreadable cached manifest ({}); recovering runtime manifest.",
                    err.message
                ));
                None
            }
        };
        let mut manifest = match found_manifest {
            Some(found) => found,
            None if registered
                && matches!(
                    plan,
                    SetupPlan::StartExisting | SetupPlan::AlreadyReady | SetupPlan::ResumeImport
                ) =>
            {
                self.log_line("Host runtime manifest missing; recovering manifest from existing WSL distribution.");
                match assets::recover_manifest_from_wsl(&wsl, &paths, saved_state.as_ref(), &redactor) {
                    Ok(found) => found,
                    Err(recover_err) => {
                        self.log_line(&format!(
                            "WSL manifest recovery skipped ({}); fetching official Windows runtime release.",
                            recover_err.message
                        ));
                        self.transition_status(
                            Phase::PreparingStorage,
                            "Fetching Nod Windows runtime information…",
                            Some(0.10),
                        );
                        assets::fetch_latest_windows_runtime_manifest(&paths, &redactor)?
                    }
                }
            }
            None => {
                self.log_line("Local runtime manifest missing; checking official Windows runtime release.");
                self.transition_status(
                    Phase::PreparingStorage,
                    "Fetching Nod Windows runtime information…",
                    Some(0.10),
                );
                assets::fetch_latest_windows_runtime_manifest(&paths, &redactor)?
            }
        };

        let needs_download_or_import = match plan {
            SetupPlan::Fresh => true,
            SetupPlan::ResumeImport => !registered,
            SetupPlan::StartExisting | SetupPlan::AlreadyReady => false,
        };
        if needs_download_or_import && manifest.guest.bytes == 0 {
            self.log_line("Cached manifest has no guest archive metadata; fetching official Windows runtime release.");
            self.transition_status(
                Phase::PreparingStorage,
                "Fetching Nod Windows runtime information…",
                Some(0.10),
            );
            manifest = assets::fetch_latest_windows_runtime_manifest(&paths, &redactor)?;
        }
        if needs_download_or_import {
            let required = assets::required_bytes(&manifest);
            storage::check_space(&paths.computer_root, required)?;
        } else {
            // Ensure free check isn't skipped unexpectedly when starting existing distro
            let _ = manifest.installed_bytes();
        }

        let mut persisted = match plan {
            SetupPlan::Fresh => {
                manifest = self.prepare_fresh_assets(&paths, manifest, &redactor)?;
                let expanded_tar = self.expand_guest_for_import(&manifest, &redactor)?;
                let mut state_val = PersistedState::new(
                    paths.distribution.clone(),
                    manifest.guest.sha256.clone(),
                );
                state_val.computer_path = Some(paths.computer_root.to_string_lossy().to_string());
                save_persisted_state(&paths.state_path, &state_val, &redactor)?;
                self.import_distro(&wsl, &paths, &expanded_tar, &redactor)?;
                assets::cleanup_expanded_image(Path::new(&expanded_tar));
                state_val.initializing = false;
                save_persisted_state(&paths.state_path, &state_val, &redactor)?;
                state_val
            }
            SetupPlan::ResumeImport => {
                let mut state_val = saved_state.ok_or_else(RuntimeError::configuration_missing)?;
                if registered {
                    state_val.initializing = false;
                    save_persisted_state(&paths.state_path, &state_val, &redactor)?;
                } else {
                    manifest = self.prepare_fresh_assets(&paths, manifest, &redactor)?;
                    let expanded_tar = self.expand_guest_for_import(&manifest, &redactor)?;
                    self.import_distro(&wsl, &paths, &expanded_tar, &redactor)?;
                    assets::cleanup_expanded_image(Path::new(&expanded_tar));
                    state_val.initializing = false;
                    save_persisted_state(&paths.state_path, &state_val, &redactor)?;
                }
                state_val
            }
            SetupPlan::StartExisting | SetupPlan::AlreadyReady => {
                saved_state.ok_or_else(RuntimeError::configuration_missing)?
            }
        };

        let redactor = redactor.secret(&persisted.password);
        let identity = storage::identity_for(&paths.distribution);
        let port = match persisted.port {
            Some(p) if p > 0 => p,
            _ => {
                let p = storage::available_port()?;
                persisted.port = Some(p);
                save_persisted_state(&paths.state_path, &persisted, &redactor)?;
                p
            }
        };

        let docker = DockerTransport::for_manifest(
            &manifest,
            paths.distribution.clone(),
            paths.control_root.join("docker-config"),
            Some(persisted.password.clone()),
        );

        if plan == SetupPlan::AlreadyReady {
            let probe = health::probe_all(&docker, &identity, Some(port));
            if probe.all_ready() {
                self.mark_ready(persisted, probe, &paths.state_path, &redactor)?;
                return Ok(());
            }
        }

        self.start_guest_and_docker(&wsl, &paths.distribution, &identity, &docker, &redactor)?;
        self.start_services(
            &wsl,
            &manifest,
            &paths,
            &identity,
            port,
            &persisted.password,
            &docker,
            &redactor,
        )?;

        self.transition_status(
            Phase::CheckingHealth,
            "Verifying workspace, database, and private browser…",
            Phase::CheckingHealth.progress_start(),
        );
        let health_report = health::wait_until_ready(
            &docker,
            &identity,
            port,
            Duration::from_secs(90),
            &redactor,
            &|| self.cancelled.load(Ordering::SeqCst),
        )?;
        self.mark_ready(persisted, health_report, &paths.state_path, &redactor)?;
        Ok(())
    }

    fn prepare_fresh_assets(
        &self,
        paths: &RuntimePaths,
        mut manifest: RuntimeManifest,
        redactor: &Redactor,
    ) -> Result<RuntimeManifest, RuntimeError> {
        if !manifest.guest_path().is_file() {
            self.transition_status(
                Phase::DownloadingRuntime,
                "Downloading private computer runtime…",
                Phase::DownloadingRuntime.progress_start(),
            );
            let dl_attempt = assets::download_guest(
                &manifest,
                redactor,
                |fraction, seconds| {
                    self.update_progress(
                        Phase::DownloadingRuntime,
                        "Downloading private computer runtime…",
                        0.16 + fraction * 0.18,
                        seconds,
                    );
                },
                || self.cancelled.load(Ordering::SeqCst),
            );
            if let Err(first_err) = dl_attempt {
                self.log_line(&format!(
                    "Initial runtime download failed ({}); refreshing latest Windows release descriptor.",
                    first_err.message
                ));
                manifest = assets::fetch_latest_windows_runtime_manifest(paths, redactor)?;
                assets::download_guest(
                    &manifest,
                    redactor,
                    |fraction, seconds| {
                        self.update_progress(
                            Phase::DownloadingRuntime,
                            "Downloading private computer runtime…",
                            0.16 + fraction * 0.18,
                            seconds,
                        );
                    },
                    || self.cancelled.load(Ordering::SeqCst),
                )?;
            }
        }
        self.transition_status(
            Phase::VerifyingRuntime,
            "Verifying runtime files…",
            Phase::VerifyingRuntime.progress_start(),
        );
        assets::verify_runtime_files(&manifest, redactor, |p, msg| {
            self.update_progress(Phase::VerifyingRuntime, &msg, 0.34 + p * 0.10, None);
        })?;
        Ok(manifest)
    }

    fn expand_guest_for_import(
        &self,
        manifest: &RuntimeManifest,
        redactor: &Redactor,
    ) -> Result<String, RuntimeError> {
        self.transition_status(
            Phase::VerifyingRuntime,
            "Preparing private computer image…",
            Some(0.40),
        );
        assets::decompress_guest(
            manifest,
            redactor,
            |p| {
                self.update_progress(
                    Phase::VerifyingRuntime,
                    "Preparing private computer image…",
                    0.35 + p * 0.10,
                    None,
                );
            },
            || self.cancelled.load(Ordering::SeqCst),
        )
    }

    fn import_distro(
        &self,
        wsl: &Wsl,
        paths: &RuntimePaths,
        guest_tar: &str,
        _redactor: &Redactor,
    ) -> Result<(), RuntimeError> {
        self.transition_status(
            Phase::ImportingDistribution,
            "Creating Nod’s private computer…",
            Phase::ImportingDistribution.progress_start(),
        );
        let dir = paths.computer_root.to_string_lossy().to_string();
        wsl.import(&paths.distribution, &dir, guest_tar)
    }

    fn start_guest_and_docker(
        &self,
        wsl: &Wsl,
        distribution: &str,
        identity: &str,
        docker: &DockerTransport,
        redactor: &Redactor,
    ) -> Result<(), RuntimeError> {
        self.transition_status(
            Phase::StartingGuest,
            "Starting private computer…",
            Phase::StartingGuest.progress_start(),
        );
        self.ensure_holder(wsl, distribution)?;
        let payload = json!({ "identity": identity }).to_string();
        let out = wsl
            .guest_stdin(
                distribution,
                &[
                    "/usr/bin/python3",
                    "/usr/local/lib/nod/windows-guest.py",
                    "start",
                ],
                &payload,
                Duration::from_secs(330),
            )
            .map_err(|err| RuntimeError::command_failed("guest", "Could not start guest VM.", err, None, redactor))?;

        if is_missing_wsl_exit(out.code) {
            return Err(RuntimeError::wsl_required());
        }
        if !out.success() {
            let combined = out.combined();
            if is_unrepairable_wsl_code(&combined) {
                return Err(RuntimeError::command_failed(
                    "guest",
                    "Windows could not start the private computer.",
                    combined,
                    out.code.map(i64::from),
                    redactor,
                ));
            }
            return Err(RuntimeError::wsl_broken(
                parse_wsl_code(&combined),
                combined,
                redactor,
            ));
        }
        health::check_docker(docker)
            .map_err(|err| RuntimeError::not_ready(err, redactor))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn start_services(
        &self,
        wsl: &Wsl,
        manifest: &RuntimeManifest,
        paths: &RuntimePaths,
        identity: &str,
        port: u16,
        _password: &str,
        docker: &DockerTransport,
        redactor: &Redactor,
    ) -> Result<(), RuntimeError> {
        self.transition_status(
            Phase::StartingServices,
            "Starting workspace and private browser services…",
            Phase::StartingServices.progress_start(),
        );
        for image in &manifest.images {
            let inspect = docker.run(
                &["image", "inspect", "--format", "{{.Id}}", &image.reference],
                Duration::from_secs(10),
                false,
            );
            let loaded = inspect
                .map(|o| o.success() && o.stdout.trim() == image.id)
                .unwrap_or(false);
            if !loaded {
                let payload = json!({ "identity": identity, "name": image.name }).to_string();
                let out = wsl
                    .guest_stdin(
                        &paths.distribution,
                        &[
                            "/usr/bin/python3",
                            "/usr/local/lib/nod/windows-guest.py",
                            "image",
                        ],
                        &payload,
                        Duration::from_secs(600),
                    )
                    .map_err(|err| {
                        RuntimeError::command_failed(
                            "services",
                            format!("Could not load {} container image.", image.name),
                            err,
                            None,
                            redactor,
                        )
                    })?;
                if !out.success() {
                    return Err(RuntimeError::not_ready(out.combined(), redactor));
                }
            }
        }

        ensure_network(docker, &control_network(identity), "172.31.80.0/24", "nod-control", redactor)?;
        ensure_network(docker, &workspace_network(identity), "172.31.81.0/24", "nod-workspace", redactor)?;
        ensure_network(docker, &browser_network(identity), "172.31.82.0/24", "nod-browser", redactor)?;

        let fw_out = wsl
            .guest_stdin(
                &paths.distribution,
                &["/bin/sh", "-s"],
                PROTECT_NETWORKS_SCRIPT,
                Duration::from_secs(30),
            )
            .map_err(|err| RuntimeError::command_failed("network", "Could not apply runtime network isolation.", err, None, redactor))?;
        if !fw_out.success() {
            return Err(RuntimeError::new(
                NOD_COMPUTER_NETWORK_INVALID,
                "Nod could not isolate its private container networks.",
            )
            .step("network")
            .details(fw_out.combined(), redactor));
        }

        for vol in [
            format!("nod-postgres-{identity}"),
            format!("nod-workspace-{identity}"),
            format!("nod-tools-{identity}"),
            format!("nod-browser-{identity}"),
        ] {
            let _ = docker.run(&["volume", "create", "--label", LABEL_MANAGED_BY, &vol], Duration::from_secs(15), false);
        }

        ensure_postgres_container(docker, manifest, identity, port, redactor)?;
        ensure_workspace_container(docker, manifest, identity, redactor)?;
        ensure_browser_container(docker, manifest, identity, redactor)?;
        Ok(())
    }

    fn ensure_holder(&self, wsl: &Wsl, distribution: &str) -> Result<(), RuntimeError> {
        let mut guard = self.holder.lock().map_err(|_| {
            RuntimeError::new(NOD_WSL_COMMAND_FAILED, "Holder process lock unavailable.")
        })?;
        if let Some(child) = guard.as_mut()
            && let Ok(None) = child.try_wait() {
                return Ok(());
            }
        *guard = Some(wsl.spawn_holder(distribution)?);
        Ok(())
    }

    fn mark_ready(
        &self,
        mut persisted: PersistedState,
        report: ServiceHealth,
        state_path: &Path,
        redactor: &Redactor,
    ) -> Result<(), RuntimeError> {
        persisted.initializing = false;
        persisted.services_ready = true;
        persisted.ready_at = Some(storage::now_iso());
        save_persisted_state(state_path, &persisted, redactor)?;
        if let Ok(mut guard) = self.persisted.lock() {
            *guard = Some(persisted);
        }
        if let Ok(mut guard) = self.latest_health.lock() {
            *guard = Some(report.clone());
        }
        if let Ok(mut guard) = self.status.lock() {
            guard.health = Some(report.to_json());
            guard.transition(Phase::Ready, "Private computer is ready.", Some(1.0));
        }
        self.broadcast_status();
        self.log_line("Private computer is ready.");
        Ok(())
    }

    fn transition_status(&self, phase: Phase, message: &str, progress: Option<f64>) {
        if let Ok(mut guard) = self.status.lock() {
            guard.transition(phase, message, progress);
        }
        self.log_line(message);
        self.broadcast_status();
    }

    fn update_progress(&self, phase: Phase, message: &str, progress: f64, seconds_left: Option<u64>) {
        if let Ok(mut guard) = self.status.lock() {
            guard.transition(phase, message, Some(progress.clamp(0.0, 1.0)));
            guard.seconds_left = seconds_left;
        }
        self.broadcast_status();
    }

    fn fail_status(&self, failure: &RuntimeError) {
        let redactor = self.redactor();
        let redacted_msg = redactor.message(&failure.message);
        let mut sanitized = failure.clone();
        sanitized.message = redacted_msg;
        sanitized.details = redactor.redact(&failure.details, 32 * 1024);
        if let Ok(mut guard) = self.status.lock() {
            guard.fail(&sanitized);
        }
        self.log_line(&format!(
            "ERROR [{}]: {} ({})",
            sanitized.code, sanitized.message, sanitized.details
        ));
        self.broadcast_status();
    }

    fn broadcast_status(&self) {
        let payload = self.status_json();
        self.live.notify("runtime");
        self.live.notify("session");
        self.push_bridge("nod:status", payload);
    }

    fn push_bridge(&self, channel: &str, payload: Value) {
        if let Ok(guard) = self.bridge.lock()
            && let Some(cb) = guard.as_ref() {
                cb(channel, payload);
            }
    }

    fn paths_snapshot(&self) -> RuntimePaths {
        self.paths
            .read()
            .map(|p| p.clone())
            .unwrap_or_else(|_| RuntimePaths::discover())
    }

    fn redactor(&self) -> Redactor {
        let paths = self.paths_snapshot();
        let mut redactor = Redactor::default()
            .with_path(paths.runtime_root.to_string_lossy(), "<runtime>")
            .with_path(paths.computer_root.to_string_lossy(), "<computer>");
        if let Ok(profile) = std::env::var("USERPROFILE") {
            redactor = redactor.with_path(profile, "<user>");
        }
        if let Ok(guard) = self.persisted.lock()
            && let Some(state) = guard.as_ref() {
                redactor = redactor.secret(&state.password);
            }
        redactor
    }

    fn log_line(&self, line: &str) {
        let path = self.startup_log_path();
        let redactor = self.redactor();
        storage::append_log_line(&path, &redactor.message(line));
    }
}

impl Drop for RuntimeManager {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.holder.lock()
            && let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
    }
}

fn has_existing_vhdx(paths: &RuntimePaths, saved: &PersistedState) -> bool {
    if paths.computer_root.join("ext4.vhdx").is_file() {
        return true;
    }
    if let Some(custom) = saved.computer_path.as_deref()
        && Path::new(custom).join("ext4.vhdx").is_file()
    {
        return true;
    }
    false
}

fn read_persisted_state(path: &Path) -> Option<PersistedState> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_persisted_state(
    path: &Path,
    state: &PersistedState,
    redactor: &Redactor,
) -> Result<(), RuntimeError> {
    let bytes = serde_json::to_vec_pretty(state).map_err(|err| {
        RuntimeError::configuration_invalid(err.to_string(), redactor)
    })?;
    storage::atomic_write(path, &bytes).map_err(|err| {
        RuntimeError::new(
            errors::NOD_COMPUTER_STORAGE_FULL,
            format!("Could not save runtime state: {err}"),
        )
        .step("storage")
    })
}

fn ensure_network(
    docker: &DockerTransport,
    name: &str,
    subnet: &str,
    bridge_name: &str,
    redactor: &Redactor,
) -> Result<(), RuntimeError> {
    let inspect = docker.run(&["network", "inspect", name], Duration::from_secs(10), false);
    if inspect.map(|o| o.success()).unwrap_or(false) {
        return Ok(());
    }
    let out = docker
        .run(
            &[
                "network",
                "create",
                "--driver",
                "bridge",
                "--subnet",
                subnet,
                "--opt",
                "com.docker.network.bridge.enable_icc=false",
                "--opt",
                &format!("com.docker.network.bridge.name={bridge_name}"),
                "--label",
                LABEL_MANAGED_BY,
                name,
            ],
            Duration::from_secs(20),
            false,
        )
        .map_err(|err| {
            RuntimeError::new(NOD_COMPUTER_NETWORK_INVALID, format!("Could not create network {name}: {err}"))
                .step("network")
        })?;
    if !out.success() {
        return Err(RuntimeError::new(
            NOD_COMPUTER_NETWORK_INVALID,
            format!("Docker network {name} failed to create."),
        )
        .step("network")
        .details(out.combined(), redactor));
    }
    Ok(())
}

fn ensure_postgres_container(
    docker: &DockerTransport,
    manifest: &RuntimeManifest,
    identity: &str,
    port: u16,
    redactor: &Redactor,
) -> Result<(), RuntimeError> {
    let name = postgres_container(identity);
    let net = control_network(identity);
    let image = manifest
        .image("postgres")
        .ok_or_else(|| RuntimeError::archive_required("Missing postgres image metadata."))?;
    let port_map = format!("127.0.0.1:{port}:5432");
    let vol_mount = format!("nod-postgres-{identity}:/var/lib/postgresql/data");
    if let Ok(true) = health::container_running(docker, &name) {
        if let Ok(port_out) = docker.run(&["port", &name, "5432/tcp"], Duration::from_secs(8), false)
            && health::verify_loopback_port_binding(&port_out.stdout, port) {
                return Ok(());
            }
        let _ = docker.run(&["rm", "-f", &name], Duration::from_secs(15), false);
    } else if docker
        .run(&["inspect", &name], Duration::from_secs(8), false)
        .map(|o| o.success())
        .unwrap_or(false)
    {
        if docker
            .run(&["start", &name], Duration::from_secs(20), false)
            .map(|o| o.success())
            .unwrap_or(false)
            && let Ok(port_out) = docker.run(&["port", &name, "5432/tcp"], Duration::from_secs(8), false)
                && health::verify_loopback_port_binding(&port_out.stdout, port) {
                    return Ok(());
                }
        let _ = docker.run(&["rm", "-f", &name], Duration::from_secs(15), false);
    }

    let out = docker
        .run(
            &[
                "run",
                "-d",
                "--name",
                &name,
                "--label",
                LABEL_MANAGED_BY,
                "--network",
                &net,
                "--publish",
                &port_map,
                "--memory",
                "256m",
                "--memory-swap",
                "256m",
                "--cpus",
                "1",
                "--volume",
                &vol_mount,
                "-e",
                "POSTGRES_USER=nod",
                "-e",
                "POSTGRES_DB=nod",
                "-e",
                "POSTGRES_PASSWORD",
                &image.reference,
                "-c",
                "shared_buffers=16MB",
                "-c",
                "work_mem=2MB",
                "-c",
                "maintenance_work_mem=16MB",
                "-c",
                "max_connections=25",
            ],
            Duration::from_secs(30),
            true,
        )
        .map_err(|err| RuntimeError::not_ready(err, redactor))?;
    if !out.success() {
        return Err(RuntimeError::not_ready(out.combined(), redactor));
    }
    Ok(())
}

fn ensure_workspace_container(
    docker: &DockerTransport,
    manifest: &RuntimeManifest,
    identity: &str,
    redactor: &Redactor,
) -> Result<(), RuntimeError> {
    let name = workspace_container(identity);
    let net = workspace_network(identity);
    let image = manifest
        .image("workspace")
        .ok_or_else(|| RuntimeError::archive_required("Missing workspace image metadata."))?;
    let ws_vol = format!("nod-workspace-{identity}:/workspace");
    let tools_vol = format!("nod-tools-{identity}:/home/node");
    if container_ensure_or_remove(docker, &name) {
        return Ok(());
    }
    let ws_mem = std::env::var("NOD_WORKSPACE_MEMORY").unwrap_or_else(|_| "1g".to_string());
    let out = docker
        .run(
            &[
                "run",
                "-d",
                "--name",
                &name,
                "--label",
                LABEL_MANAGED_BY,
                "--init",
                "--network",
                &net,
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges:true",
                "--pids-limit",
                "512",
                "--memory",
                &ws_mem,
                "--memory-swap",
                &ws_mem,
                "--cpus",
                "2",
                "--volume",
                &ws_vol,
                "--volume",
                &tools_vol,
                "--entrypoint",
                "/usr/bin/sleep",
                &image.reference,
                "infinity",
            ],
            Duration::from_secs(30),
            false,
        )
        .map_err(|err| RuntimeError::not_ready(err, redactor))?;
    if !out.success() {
        return Err(RuntimeError::not_ready(out.combined(), redactor));
    }
    Ok(())
}

fn ensure_browser_container(
    docker: &DockerTransport,
    manifest: &RuntimeManifest,
    identity: &str,
    redactor: &Redactor,
) -> Result<(), RuntimeError> {
    let name = browser_container(identity);
    let net = browser_network(identity);
    let image = manifest
        .image("browser")
        .ok_or_else(|| RuntimeError::archive_required("Missing browser image metadata."))?;
    let data_vol = format!("nod-browser-{identity}:/data");
    if container_ensure_or_remove(docker, &name) {
        return Ok(());
    }
    let browser_mem = std::env::var("NOD_BROWSER_MEMORY").unwrap_or_else(|_| "1g".to_string());
    let out = docker
        .run(
            &[
                "run",
                "-d",
                "--name",
                &name,
                "--label",
                LABEL_MANAGED_BY,
                "--init",
                "--network",
                &net,
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges:true",
                "--read-only",
                "--pids-limit",
                "512",
                "--memory",
                &browser_mem,
                "--memory-swap",
                &browser_mem,
                "--cpus",
                "2",
                "--shm-size",
                "256m",
                "--tmpfs",
                "/tmp:size=512m",
                "--tmpfs",
                "/home/node:uid=1000,gid=1000,mode=700,size=128m",
                "--volume",
                &data_vol,
                &image.reference,
            ],
            Duration::from_secs(30),
            false,
        )
        .map_err(|err| RuntimeError::not_ready(err, redactor))?;
    if !out.success() {
        return Err(RuntimeError::not_ready(out.combined(), redactor));
    }
    Ok(())
}

fn container_ensure_or_remove(docker: &DockerTransport, name: &str) -> bool {
    if health::container_running(docker, name).unwrap_or(false) {
        return true;
    }
    if docker
        .run(&["inspect", name], Duration::from_secs(8), false)
        .map(|o| o.success())
        .unwrap_or(false)
    {
        if docker
            .run(&["start", name], Duration::from_secs(20), false)
            .map(|o| o.success())
            .unwrap_or(false)
        {
            return true;
        }
        let _ = docker.run(&["rm", "-f", name], Duration::from_secs(15), false);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_and_preferences_round_trip() {
        let dir = std::env::temp_dir().join(format!("nod-rt-test-{}", storage::random_suffix(8)));
        let _ = fs::remove_dir_all(&dir);
        let paths = RuntimePaths {
            data_dir: dir.clone(),
            runtime_root: dir.clone(),
            control_root: dir.join("desktop-runtime"),
            computer_root: dir.join("desktop-runtime").join("wsl"),
            logs_root: dir.join("logs"),
            settings_path: dir.join("settings.json"),
            preferences_path: dir.join("preferences.json"),
            chat_read_cursors_path: dir.join("chat-read-cursors.json"),
            state_path: dir.join("desktop-runtime").join("state.json"),
            startup_log_path: dir.join("logs").join("startup.log"),
            distribution: "nod-test123456".into(),
        };
        let live = Arc::new(Live::new());
        let manager = Arc::new(RuntimeManager {
            paths: RwLock::new(paths),
            status: Mutex::new(Status::idle("Idle", Some("nod-test123456".into()))),
            setup_gate: (Mutex::new(false), Condvar::new()),
            cancelled: AtomicBool::new(false),
            holder: Mutex::new(None),
            persisted: Mutex::new(None),
            latest_health: Mutex::new(None),
            live,
            bridge: Mutex::new(None),
        });
        assert!(manager.save_preferences(json!({ "theme": "dark" })).is_ok());
        assert_eq!(manager.get_preferences()["theme"], json!("dark"));
        assert_eq!(manager.get_preferences()["resolvedTheme"], json!("dark"));
        assert!(manager.save_preferences(json!({ "theme": "neon" })).is_err());

        let saved = manager
            .save_settings(json!({ "onboardingPreferenceStep": true }))
            .unwrap();
        assert_eq!(saved["onboardingPreferenceStep"], json!("complete"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_mapping_and_busy_lock_reflect_real_contract() {
        let dir = std::env::temp_dir().join(format!("nod-rt-err-{}", storage::random_suffix(8)));
        let _ = fs::remove_dir_all(&dir);
        let paths = RuntimePaths {
            data_dir: dir.clone(),
            runtime_root: dir.clone(),
            control_root: dir.join("desktop-runtime"),
            computer_root: dir.join("desktop-runtime").join("wsl"),
            logs_root: dir.join("logs"),
            settings_path: dir.join("settings.json"),
            preferences_path: dir.join("preferences.json"),
            chat_read_cursors_path: dir.join("chat-read-cursors.json"),
            state_path: dir.join("desktop-runtime").join("state.json"),
            startup_log_path: dir.join("logs").join("startup.log"),
            distribution: "nod-test654321".into(),
        };
        let live = Arc::new(Live::new());
        let manager = Arc::new(RuntimeManager {
            paths: RwLock::new(paths),
            status: Mutex::new(Status::idle("Idle", Some("nod-test654321".into()))),
            setup_gate: (Mutex::new(false), Condvar::new()),
            cancelled: AtomicBool::new(false),
            holder: Mutex::new(None),
            persisted: Mutex::new(None),
            latest_health: Mutex::new(None),
            live,
            bridge: Mutex::new(None),
        });

        assert!(!manager.is_ready());
        assert!(!manager.browser_ready());

        manager.fail_status(&RuntimeError::wsl_required());
        let wsl_json = manager.status_json();
        assert_eq!(wsl_json["phase"], json!("error"));
        assert_eq!(wsl_json["state"], json!("needs_wsl"));
        assert_eq!(wsl_json["needsWsl"], json!(true));
        assert_eq!(wsl_json["engineReady"], json!(false));

        manager.fail_status(&RuntimeError::storage_full(1024, 10_000_000_000));
        let disk_json = manager.status_json();
        assert_eq!(disk_json["needsStorage"], json!(true));
        assert_eq!(disk_json["state"], json!("error"));

        let res = manager.prepare(false);
        assert_eq!(res.state, Phase::Error);
        assert!(!manager.is_ready());
        let _ = fs::remove_dir_all(&dir);
    }
}
