use serde::Deserialize;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum HostEvent {
    Status {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        progress: Option<f64>,
    },
    EngineReady {
        origin: String,
    },
    Ready,
    Working {
        working: bool,
    },
    EngineExit {
        code: Option<i32>,
        #[serde(default)]
        message: String,
    },
    Error {
        message: String,
    },
    Stopped,
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub install: PathBuf,
    pub user_data: PathBuf,
    pub profile: PathBuf,
    pub host_script: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self, String> {
        let local = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set.")?;
        let roaming = std::env::var_os("APPDATA").ok_or("APPDATA is not set.")?;
        let install = std::env::var_os("NOD_INSTALL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(local).join("Programs").join("nod"));
        if !install.join("nod.exe").is_file() {
            return Err(format!(
                "Nod is not installed at {}. Install the original Nod app first; this app reuses its engine.",
                install.display()
            ));
        }
        let user_data = PathBuf::from(roaming).join("nod");
        let profile = user_data.join("profiles").join("local");
        if !profile.join("profile.json").is_file() {
            return Err("Finish Nod setup in the original app once, then open this app.".into());
        }
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_default();
        let host_script = [
            exe_dir.join("host").join("nod-host.mjs"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("host")
                .join("nod-host.mjs"),
        ]
        .into_iter()
        .find(|p| p.is_file())
        .ok_or("The Nod host script is missing next to the app.")?;
        Ok(Self {
            install,
            user_data,
            profile,
            host_script,
        })
    }

    pub fn engine_root(&self) -> PathBuf {
        self.install.join("resources").join("engine")
    }

    pub fn app_version(&self) -> String {
        std::fs::read_to_string(self.install.join("resources").join("app").join("package.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["version"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "0.0.0".into())
    }
}

pub struct Host {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Host {
    pub fn spawn(
        paths: &Paths,
        token: &str,
        vault_key: &str,
        port: u16,
    ) -> Result<(Self, mpsc::UnboundedReceiver<HostEvent>), String> {
        let mut command = Command::new(paths.install.join("nod.exe"));
        command
            .arg(&paths.host_script)
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| {
                matches!(
                    k.to_ascii_uppercase().as_str(),
                    "PATH"
                        | "SYSTEMROOT"
                        | "USERPROFILE"
                        | "LOCALAPPDATA"
                        | "APPDATA"
                        | "TEMP"
                        | "TMP"
                        | "HOME"
                        | "LANG"
                        | "COMSPEC"
                        | "WINDIR"
                        | "PROGRAMDATA"
                )
            }))
            .env("ELECTRON_RUN_AS_NODE", "1")
            .env("NOD_HOST_ENGINE_ROOT", paths.engine_root())
            .env("NOD_HOST_PROFILE", &paths.profile)
            .env("NOD_HOST_TOKEN", token)
            .env("NOD_HOST_VAULT_KEY", vault_key)
            .env("NOD_HOST_PORT", port.to_string())
            .env("NOD_HOST_APP_VERSION", paths.app_version())
            .current_dir(paths.engine_root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log_file())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        let mut child = command
            .spawn()
            .map_err(|e| format!("Nod could not start its engine: {e}"))?;
        let stdout = child.stdout.take().ok_or("The engine has no output.")?;
        let stdin = child.stdin.take();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(event) = serde_json::from_str::<HostEvent>(&line) {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            }
        });
        Ok((Self { child, stdin }, rx))
    }

    pub async fn quit(&mut self) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin.write_all(b"{\"type\":\"quit\"}\n").await;
            let _ = stdin.flush().await;
        }
        let wait = tokio::time::timeout(std::time::Duration::from_secs(60), self.child.wait());
        if wait.await.is_err() {
            let _ = self.child.kill().await;
        }
    }
}

fn log_file() -> Stdio {
    let Some(dir) = std::env::var_os("APPDATA")
        .map(|d| PathBuf::from(d).join("nod-rs").join("logs"))
    else {
        return Stdio::null();
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return Stdio::null();
    }
    let path = dir.join("host.log");
    if std::fs::metadata(&path).map(|m| m.len() > 4 << 20).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join("host.old.log"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null())
}

pub fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("Nod could not find a free local port: {e}"))
}

pub fn original_app_running(install: &std::path::Path) -> bool {
    let original = install.join("nod.exe");
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::nothing()
            .with_cmd(sysinfo::UpdateKind::Always)
            .with_exe(sysinfo::UpdateKind::Always),
    );
    system.processes().values().any(|p| {
        p.exe()
            .map(|exe| exe.to_string_lossy().eq_ignore_ascii_case(&original.to_string_lossy()))
            .unwrap_or(false)
            && !p.cmd().iter().any(|a| {
                let a = a.to_string_lossy();
                a.starts_with("--type=") || a.ends_with(".mjs")
            })
    })
}
