//! Guest Docker transport and container readiness probes.

use crate::runtime::errors::{Redactor, RuntimeError};
use crate::runtime::{storage, windows, wsl};
use serde::Serialize;
use serde_json::{json, Value};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

const BROWSER_SOCKET_CHECK: &str = concat!(
    "const net = require('node:net');",
    "const s = net.connect('/data/control.sock');",
    "s.on('connect', () => { s.end(); process.exit(0); });",
    "s.on('error', () => process.exit(1));",
    "setTimeout(() => process.exit(2), 3000);"
);

/// Configuration to invoke Docker inside the WSL distribution (via `bin/docker.exe` or direct `wsl.exe`).
#[derive(Debug, Clone)]
pub struct DockerTransport {
    pub exe: PathBuf,
    pub distribution: String,
    pub docker_config: PathBuf,
    pub postgres_password: Option<String>,
}

impl DockerTransport {
    /// Selects bundled `bin/docker.exe` when present, falling back to trusted direct `wsl.exe` execution.
    pub fn for_manifest(
        manifest: &crate::runtime::assets::RuntimeManifest,
        distribution: String,
        docker_config: PathBuf,
        postgres_password: Option<String>,
    ) -> Self {
        let candidate = manifest.transport_path();
        let exe = if !manifest.uses_wsl_transport() && candidate.is_file() {
            candidate
        } else {
            windows::wsl_exe()
        };
        Self {
            exe,
            distribution,
            docker_config,
            postgres_password,
        }
    }

    fn uses_direct_wsl(&self) -> bool {
        self.exe == windows::wsl_exe() || !self.exe.is_file()
    }

    /// Builds the clean environment vector expected by `wsl-docker_windows.go` and `wsl.exe`.
    pub fn environment(&self, include_postgres_password: bool) -> Vec<(String, String)> {
        let system_root = windows::system_root();
        let system32 = system_root.join("System32");
        let mut env = Vec::new();
        for key in [
            "SYSTEMROOT",
            "USERPROFILE",
            "LOCALAPPDATA",
            "TEMP",
            "TMP",
            "PSModuleAnalysisCachePath",
        ] {
            if let Ok(value) = std::env::var(key)
                && !value.is_empty() {
                    env.push((key.to_string(), value));
                }
        }
        env.push((
            "SYSTEMROOT".to_string(),
            system_root.to_string_lossy().to_string(),
        ));
        env.push((
            "PATH".to_string(),
            system32.to_string_lossy().to_string(),
        ));
        env.push(("NOD_WSL_DISTRIBUTION".to_string(), self.distribution.clone()));
        env.push((
            "DOCKER_CONFIG".to_string(),
            self.docker_config.to_string_lossy().to_string(),
        ));
        env.push(("DOCKER_HOST".to_string(), String::new()));
        env.push(("DOCKER_CONTEXT".to_string(), String::new()));
        if include_postgres_password
            && let Some(password) = &self.postgres_password {
                env.push(("POSTGRES_PASSWORD".to_string(), password.clone()));
                env.push(("WSLENV".to_string(), "POSTGRES_PASSWORD".to_string()));
            }
        env
    }

    /// Runs `docker <args>` inside a cleared environment with bounded output and timeout.
    pub fn run(
        &self,
        args: &[&str],
        timeout: Duration,
        include_postgres_password: bool,
    ) -> Result<wsl::CommandOutput, String> {
        self.run_with_input(args, None, timeout, include_postgres_password)
    }

    /// Runs `docker <args>` with optional stdin input.
    pub fn run_with_input(
        &self,
        args: &[&str],
        input: Option<&str>,
        timeout: Duration,
        include_postgres_password: bool,
    ) -> Result<wsl::CommandOutput, String> {
        let env = self.environment(include_postgres_password);
        if self.uses_direct_wsl() {
            let wsl_bin = windows::wsl_exe();
            if !windows::is_trusted_windows_binary(&wsl_bin) {
                return Err("Trusted Windows wsl.exe binary is not available.".to_string());
            }
            let mut arg_strings = vec![
                "--distribution".to_string(),
                self.distribution.clone(),
                "--user".to_string(),
                "root".to_string(),
                "--exec".to_string(),
                "/usr/local/bin/nod-docker".to_string(),
            ];
            arg_strings.extend(args.iter().map(|s| (*s).to_string()));
            wsl::run_command(&wsl_bin, &arg_strings, &env, timeout, input)
        } else {
            let arg_strings: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
            wsl::run_command(&self.exe, &arg_strings, &env, timeout, input)
        }
    }
}

/// Aggregated health report across all required guest services.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceHealth {
    pub docker_ready: bool,
    pub postgres_ready: bool,
    pub postgres_port: Option<u16>,
    pub workspace_ready: bool,
    pub browser_ready: bool,
    pub checked_at: String,
    pub detail: Option<String>,
}

impl ServiceHealth {
    /// Returns true only when Docker and all three required services pass live health probes.
    pub fn all_ready(&self) -> bool {
        self.docker_ready && self.postgres_ready && self.workspace_ready && self.browser_ready
    }

    /// Converts the health snapshot to JSON for API and bridge status payloads.
    pub fn to_json(&self) -> Value {
        json!({
            "docker": self.docker_ready,
            "postgres": self.postgres_ready,
            "postgresPort": self.postgres_port,
            "workspace": self.workspace_ready,
            "browser": self.browser_ready,
            "allReady": self.all_ready(),
            "checkedAt": self.checked_at,
            "detail": self.detail,
        })
    }
}

pub fn postgres_container(identity: &str) -> String {
    format!("nod-postgres-{identity}")
}

pub fn workspace_container(identity: &str) -> String {
    format!("nod-workspace-{identity}")
}

pub fn browser_container(identity: &str) -> String {
    format!("nod-browser-{identity}")
}

pub fn control_network(identity: &str) -> String {
    format!("nod-control-{identity}")
}

pub fn workspace_network(identity: &str) -> String {
    format!("nod-workspace-{identity}")
}

pub fn browser_network(identity: &str) -> String {
    format!("nod-browser-{identity}")
}

/// Verifies that the guest `dockerd` daemon responds to `docker info`.
pub fn check_docker(docker: &DockerTransport) -> Result<String, String> {
    let out = docker.run(&["info", "--format", "{{.ServerVersion}}"], Duration::from_secs(10), false)?;
    if out.success() && !out.stdout.trim().is_empty() {
        Ok(out.stdout.trim().to_string())
    } else {
        Err(format!("Docker daemon not ready: {}", out.combined()))
    }
}

/// Checks whether a container is currently running via `docker inspect`.
pub fn container_running(docker: &DockerTransport, name: &str) -> Result<bool, String> {
    let out = docker.run(
        &["inspect", "-f", "{{.State.Running}}", name],
        Duration::from_secs(8),
        false,
    )?;
    if !out.success() {
        return Ok(false);
    }
    Ok(out.stdout.trim() == "true")
}

/// Parses `docker port` lines and verifies Postgres binds strictly to loopback on `expected_port`.
pub fn verify_loopback_port_binding(port_output: &str, expected_port: u16) -> bool {
    let target = format!("127.0.0.1:{expected_port}");
    let mut matched = false;
    for line in port_output.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line.starts_with("0.0.0.0:") || line.starts_with(":::") || line.starts_with("[::]:") {
            return false;
        }
        if line == target {
            matched = true;
        }
    }
    matched
}

/// Verifies Postgres container state, localhost-only port binding, `pg_isready`, and host TCP connect.
pub fn check_postgres(docker: &DockerTransport, identity: &str, port: u16) -> Result<(), String> {
    let container = postgres_container(identity);
    if !container_running(docker, &container)? {
        return Err(format!("Container {container} is not running."));
    }
    let port_out = docker.run(&["port", &container, "5432/tcp"], Duration::from_secs(8), false)?;
    if !port_out.success() || !verify_loopback_port_binding(&port_out.stdout, port) {
        return Err(format!(
            "Postgres port binding invalid or exposed: {}",
            port_out.combined()
        ));
    }
    let ready_out = docker.run(
        &[
            "exec",
            &container,
            "pg_isready",
            "-h",
            "127.0.0.1",
            "-U",
            "nod",
            "-d",
            "nod",
        ],
        Duration::from_secs(10),
        false,
    )?;
    if !ready_out.success() {
        return Err(format!("pg_isready failed: {}", ready_out.combined()));
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|err| format!("Host TCP connect to 127.0.0.1:{port} failed: {err}"))?;
    Ok(())
}

/// Verifies the isolated workspace container is running and responsive.
pub fn check_workspace(docker: &DockerTransport, identity: &str) -> Result<(), String> {
    let container = workspace_container(identity);
    if !container_running(docker, &container)? {
        return Err(format!("Container {container} is not running."));
    }
    let out = docker.run(&["exec", &container, "true"], Duration::from_secs(8), false)?;
    if !out.success() {
        return Err(format!("Workspace probe failed: {}", out.combined()));
    }
    Ok(())
}

/// Verifies the hardened private browser container is running AND `/data/control.sock` accepts connections.
pub fn check_browser(docker: &DockerTransport, identity: &str) -> Result<(), String> {
    let container = browser_container(identity);
    if !container_running(docker, &container)? {
        return Err(format!("Container {container} is not running."));
    }
    let out = docker.run(
        &["exec", &container, "node", "-e", BROWSER_SOCKET_CHECK],
        Duration::from_secs(8),
        false,
    )?;
    if !out.success() {
        return Err(format!(
            "Private browser control socket not ready: {}",
            out.combined()
        ));
    }
    Ok(())
}

/// Probes Docker, Postgres, Workspace, and Browser readiness once without mutating state.
pub fn probe_all(docker: &DockerTransport, identity: &str, postgres_port: Option<u16>) -> ServiceHealth {
    let mut details = Vec::new();
    let docker_ready = match check_docker(docker) {
        Ok(_) => true,
        Err(err) => {
            details.push(err);
            false
        }
    };
    let postgres_ready = match postgres_port {
        Some(port) if docker_ready => match check_postgres(docker, identity, port) {
            Ok(()) => true,
            Err(err) => {
                details.push(err);
                false
            }
        },
        None => {
            details.push("No allocated local Postgres port.".to_string());
            false
        }
        _ => false,
    };
    let workspace_ready = if docker_ready {
        match check_workspace(docker, identity) {
            Ok(()) => true,
            Err(err) => {
                details.push(err);
                false
            }
        }
    } else {
        false
    };
    let browser_ready = if docker_ready {
        match check_browser(docker, identity) {
            Ok(()) => true,
            Err(err) => {
                details.push(err);
                false
            }
        }
    } else {
        false
    };
    ServiceHealth {
        docker_ready,
        postgres_ready,
        postgres_port,
        workspace_ready,
        browser_ready,
        checked_at: storage::now_iso(),
        detail: if details.is_empty() {
            None
        } else {
            Some(details.join("; "))
        },
    }
}

/// Polls guest service probes until all services pass health checks or the deadline expires.
pub fn wait_until_ready(
    docker: &DockerTransport,
    identity: &str,
    postgres_port: u16,
    timeout: Duration,
    redactor: &Redactor,
    cancelled: &dyn Fn() -> bool,
) -> Result<ServiceHealth, RuntimeError> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancelled() {
            return Err(RuntimeError::not_ready("Nod setup was cancelled.", redactor));
        }
        let report = probe_all(docker, identity, Some(postgres_port));
        if report.all_ready() {
            return Ok(report);
        }
        if Instant::now() >= deadline {
            return Err(RuntimeError::not_ready(
                report
                    .detail
                    .unwrap_or_else(|| "Guest services failed readiness checks.".to_string()),
                redactor,
            ));
        }
        thread::sleep(Duration::from_millis(750));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_binding_rejects_public_interfaces() {
        assert!(verify_loopback_port_binding("127.0.0.1:54321\n", 54321));
        assert!(!verify_loopback_port_binding("0.0.0.0:54321\n", 54321));
        assert!(!verify_loopback_port_binding("[::]:54321\n", 54321));
        assert!(!verify_loopback_port_binding(
            "127.0.0.1:54321\n0.0.0.0:54321\n",
            54321
        ));
        assert!(!verify_loopback_port_binding("127.0.0.1:9999\n", 54321));
    }

    #[test]
    fn service_health_requires_every_dependency() {
        let mut health = ServiceHealth {
            docker_ready: true,
            postgres_ready: true,
            postgres_port: Some(55000),
            workspace_ready: true,
            browser_ready: false,
            checked_at: "2026-01-01T00:00:00Z".into(),
            detail: Some("control.sock pending".into()),
        };
        assert!(!health.all_ready());
        health.browser_ready = true;
        assert!(health.all_ready());
    }

    #[test]
    fn docker_environment_passes_password_only_when_requested() {
        let transport = DockerTransport {
            exe: PathBuf::from("bin/docker.exe"),
            distribution: "nod-123456789abc".into(),
            docker_config: PathBuf::from("C:\\tmp\\docker"),
            postgres_password: Some("super-secret-value".into()),
        };
        let without_pw = transport.environment(false);
        assert!(!without_pw.iter().any(|(k, _)| k == "POSTGRES_PASSWORD"));
        let with_pw = transport.environment(true);
        assert!(with_pw
            .iter()
            .any(|(k, v)| k == "POSTGRES_PASSWORD" && v == "super-secret-value"));
        assert!(with_pw
            .iter()
            .any(|(k, v)| k == "WSLENV" && v == "POSTGRES_PASSWORD"));
    }

    #[test]
    fn for_manifest_uses_wsl_when_host_transport_missing() {
        let raw = json!({
            "version": 1,
            "platform": "win32-x64",
            "transportMode": "wsl",
            "images": [
                {"name": "workspace", "reference": "nod-workspace:test", "id": format!("sha256:{}", "a".repeat(64)), "sha256": "b".repeat(64), "size": 100u64},
                {"name": "browser", "reference": "nod-browser:test", "id": format!("sha256:{}", "c".repeat(64)), "sha256": "d".repeat(64), "size": 200u64},
                {"name": "postgres", "reference": "postgres:17-alpine", "id": format!("sha256:{}", "e".repeat(64)), "sha256": "f".repeat(64), "size": 300u64}
            ]
        });
        let manifest = crate::runtime::assets::parse_guest_manifest(
            &raw,
            std::path::Path::new("."),
            None,
        )
        .unwrap();
        let transport = DockerTransport::for_manifest(
            &manifest,
            "nod-123456789abc".into(),
            PathBuf::from("C:\\tmp\\docker"),
            None,
        );
        assert!(transport.uses_direct_wsl());
        assert_eq!(transport.exe, windows::wsl_exe());
    }
}
