use crate::runtime::errors::{is_unrepairable_wsl_code, parse_wsl_code, Redactor, RuntimeError};
use crate::runtime::windows;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
#[allow(dead_code)]
const TAIL_LIMIT: usize = 32 * 1024;

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }

    pub fn combined(&self) -> String {
        let stdout = self.stdout.trim();
        let stderr = self.stderr.trim();
        match (stdout.is_empty(), stderr.is_empty()) {
            (true, true) => String::new(),
            (false, true) => stdout.to_string(),
            (true, false) => stderr.to_string(),
            (false, false) => format!("{stdout}\n{stderr}"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Distributions {
    pub registered: Vec<String>,
    pub running: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Wsl {
    pub exe: PathBuf,
    pub system_root: PathBuf,
}

impl Wsl {
    pub fn discover() -> Result<Self, RuntimeError> {
        let system_root = windows::system_root();
        if !system_root.is_dir() {
            return Err(RuntimeError::new(
                crate::runtime::errors::NOD_WSL_COMMAND_FAILED,
                "Nod could not find the Windows system folder.",
            )
            .step("wsl")
            .retryable(false));
        }
        let exe = windows::wsl_exe();
        if !windows::is_trusted_windows_binary(&exe) {
            return Err(RuntimeError::wsl_required());
        }
        Ok(Self { exe, system_root })
    }

    pub fn environment(&self) -> Vec<(String, String)> {
        let system32 = self.system_root.join("System32");
        let mut environment = Vec::new();
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
                    environment.push((key.to_string(), value));
                }
        }
        environment.push(("SYSTEMROOT".into(), self.system_root.to_string_lossy().to_string()));
        environment.push(("PATH".into(), system32.to_string_lossy().to_string()));
        environment
    }

    pub fn probe(&self, redactor: &Redactor) -> Result<String, RuntimeError> {
        let output = self
            .run(&["--status".to_string()], Duration::from_secs(15), None)
            .map_err(|error| {
                RuntimeError::wsl_broken(
                    None,
                    format!("Nod could not launch wsl.exe: {error}"),
                    redactor,
                )
            })?;
        if output.timed_out {
            return Err(RuntimeError::wsl_broken(
                None,
                "WSL did not answer the status check in time.",
                redactor,
            ));
        }
        let combined = output.combined();
        let lowercase = combined.to_lowercase();
        if output.success()
            || lowercase.contains("default version")
            || lowercase.contains("default distribution")
        {
            return Ok(combined);
        }
        if lowercase.contains("no installed distribution") {
            return Ok(combined);
        }
        if lowercase.contains("wsl")
            && (lowercase.contains("not installed")
                || lowercase.contains("optional component")
                || lowercase.contains("not enabled"))
        {
            return Err(RuntimeError::wsl_required());
        }
        Err(RuntimeError::wsl_broken(
            parse_wsl_code(&combined),
            format!(
                "WSL status failed with code {}.\n{}",
                output
                    .code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                combined
            ),
            redactor,
        ))
    }

    pub fn list_all(&self, redactor: &Redactor) -> Result<Vec<String>, RuntimeError> {
        self.list(&["--list", "--quiet", "--all"], redactor)
    }

    pub fn list_running(&self, redactor: &Redactor) -> Result<Vec<String>, RuntimeError> {
        self.list(&["--list", "--quiet"], redactor)
    }

    fn list(&self, args: &[&str], redactor: &Redactor) -> Result<Vec<String>, RuntimeError> {
        let output = self
            .run(
                &args.iter().map(|value| value.to_string()).collect::<Vec<_>>(),
                Duration::from_secs(20),
                None,
            )
            .map_err(|error| {
                RuntimeError::command_failed(
                    "wsl",
                    "Nod could not run wsl.exe to list distributions.",
                    error,
                    None,
                    redactor,
                )
            })?;
        if output.timed_out {
            return Err(RuntimeError::new(
                crate::runtime::errors::NOD_WSL_COMMAND_FAILED,
                "WSL did not answer while Nod checked its distributions.",
            )
            .step("wsl")
            .reason("The command timed out."));
        }
        let combined = output.combined();
        if output.success() {
            return Ok(parse_distributions(&output.stdout));
        }
        let lowercase = combined.to_lowercase();
        if lowercase.contains("no installed distribution")
            || lowercase.contains("no distributions")
        {
            return Ok(Vec::new());
        }
        Err(RuntimeError::command_failed(
            "wsl",
            "Nod could not read the WSL distribution list.",
            combined,
            output.code.map(i64::from),
            redactor,
        ))
    }

    pub fn distributions(&self, redactor: &Redactor) -> Result<Distributions, RuntimeError> {
        let registered = self.list_all(redactor)?;
        let running = self.list_running(redactor).unwrap_or_default();
        if running.iter().any(|name| !registered.contains(name)) {
            return Err(RuntimeError::new(
                crate::runtime::errors::NOD_WSL_COMMAND_FAILED,
                "WSL returned conflicting private computer states. Its files were kept.",
            )
            .step("wsl")
            .retryable(true));
        }
        Ok(Distributions { registered, running })
    }

    pub fn import(&self, name: &str, directory: &str, archive: &str) -> Result<(), RuntimeError> {
        let output = self
            .run(
                &[
                    "--import".to_string(),
                    name.to_string(),
                    directory.to_string(),
                    archive.to_string(),
                    "--version".to_string(),
                    "2".to_string(),
                ],
                Duration::from_secs(1_200),
                None,
            )
            .map_err(|error| RuntimeError::import_failed(error, &Redactor::default()))?;
        if output.success() {
            Ok(())
        } else {
            Err(RuntimeError::new(
                crate::runtime::errors::NOD_WSL_IMPORT_FAILED,
                if output.timed_out {
                    "Importing the Nod private computer took too long. Its previous files were kept."
                } else {
                    "Nod could not import its private computer. Its previous files were kept."
                },
            )
            .step("import")
            .needs_runtime_archive()
            .retryable(false)
            .wsl_code(parse_wsl_code(&output.combined()))
            .exit_code(output.code.map(i64::from)))
        }
    }

    pub fn terminate(&self, name: &str) -> Result<(), String> {
        let output = self
            .run(
                &["--terminate".to_string(), name.to_string()],
                Duration::from_secs(90),
                None,
            )
            .map_err(|error| error.to_string())?;
        if output.success() {
            Ok(())
        } else {
            Err(output.combined())
        }
    }

    #[allow(dead_code)]
    pub fn unregister(&self, name: &str) -> Result<(), String> {
        let output = self
            .run(
                &["--unregister".to_string(), name.to_string()],
                Duration::from_secs(300),
                None,
            )
            .map_err(|error| error.to_string())?;
        if output.success() {
            Ok(())
        } else {
            Err(output.combined())
        }
    }

    pub fn guest(
        &self,
        name: &str,
        command: &[&str],
        input: Option<&str>,
        timeout: Duration,
    ) -> Result<CommandOutput, String> {
        let mut args = vec![
            "--distribution".to_string(),
            name.to_string(),
            "--user".to_string(),
            "root".to_string(),
            "--exec".to_string(),
        ];
        args.extend(command.iter().map(|value| value.to_string()));
        self.run(&args, timeout, input)
    }

    pub fn guest_stdin(
        &self,
        name: &str,
        command: &[&str],
        input: &str,
        timeout: Duration,
    ) -> Result<CommandOutput, String> {
        self.guest(name, command, Some(input), timeout)
    }

    pub fn spawn_holder(&self, name: &str) -> Result<std::process::Child, RuntimeError> {
        let mut command = Command::new(&self.exe);
        command.args(hold_command(name));
        command.env_clear();
        for (key, value) in self.environment() {
            command.env(key, value);
        }
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        windows::hide_console(&mut command);
        command.spawn().map_err(|error| {
            RuntimeError::command_failed(
                "guest",
                "Nod could not keep its private computer running.",
                error.to_string(),
                None,
                &Redactor::default(),
            )
        })
    }

    pub fn run(
        &self,
        args: &[String],
        timeout: Duration,
        input: Option<&str>,
    ) -> Result<CommandOutput, String> {
        let environment = self.environment();
        run_command(&self.exe, args, &environment, timeout, input)
    }
}

pub fn run_command(
    executable: &PathBuf,
    args: &[String],
    environment: &[(String, String)],
    timeout: Duration,
    input: Option<&str>,
) -> Result<CommandOutput, String> {
    let mut command = Command::new(executable);
    command.args(args);
    command.env_clear();
    for (key, value) in environment {
        command.env(key, value);
    }
    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    windows::hide_console(&mut command);

    let mut child = command
        .spawn()
        .map_err(|error| format!("The command could not start: {error}"))?;

    if let Some(input) = input
        && let Some(mut stdin) = child.stdin.take() {
            let bytes = input.as_bytes().to_vec();
            thread::spawn(move || {
                let _ = stdin.write_all(&bytes);
            });
        }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    if let Some(stdout) = stdout {
        thread::spawn(move || {
            let _ = stdout_tx.send(read_limited(stdout));
        });
    }
    if let Some(stderr) = stderr {
        thread::spawn(move || {
            let _ = stderr_tx.send(read_limited(stderr));
        });
    }

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(format!("The command could not be checked: {error}")),
        }
    };
    let stdout = stdout_rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    let stderr = stderr_rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    Ok(CommandOutput {
        code: status.and_then(|status| status.code()),
        stdout: decode_windows_output(&stdout),
        stderr: decode_windows_output(&stderr),
        timed_out,
    })
}

fn read_limited(mut reader: impl Read) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if output.len() < OUTPUT_LIMIT {
                    let remaining = OUTPUT_LIMIT - output.len();
                    output.extend_from_slice(&buffer[..count.min(remaining)]);
                }
            }
            Err(_) => break,
        }
    }
    output
}

pub fn decode_windows_output(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let utf16 = bytes.len() >= 2
        && ((bytes[0] == 0xff && bytes[1] == 0xfe)
            || (bytes.len().is_multiple_of(2)
                && bytes
                    .iter()
                    .skip(1)
                    .step_by(2)
                    .filter(|byte| **byte == 0)
                    .count()
                    > bytes.len() / 6));
    let mut text = if utf16 {
        let start = if bytes.starts_with(&[0xff, 0xfe]) { 2 } else { 0 };
        let units: Vec<u16> = bytes[start..]
            .as_chunks::<2>().0.iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).to_string()
    };
    text = text.replace("\r\n", "\n").replace('\r', "\n");
    text = text.replace('\0', "");
    text.trim_matches(|character: char| character.is_whitespace() || character == '\u{feff}')
        .to_string()
}

pub fn parse_distributions(value: &str) -> Vec<String> {
    let mut distributions = Vec::new();
    for line in value.lines() {
        let line = line
            .replace('\0', "")
            .trim()
            .trim_start_matches('*')
            .trim()
            .to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('<') && line.ends_with('>') {
            continue;
        }
        let lowercase = line.to_lowercase();
        if lowercase.contains("no installed distribution")
            || lowercase.contains("no distributions")
            || lowercase.contains("windows subsystem for linux")
            || lowercase.contains("wsl version")
            || lowercase.contains("default version")
            || lowercase.contains("default distribution")
        {
            continue;
        }
        if !line.contains(' ') {
            distributions.push(line);
        }
    }
    distributions
}

#[allow(dead_code)]
pub fn parse_running(value: &str) -> Vec<String> {
    parse_distributions(value)
}

pub fn install_wsl() -> Result<(), String> {
    let powershell = windows::powershell_exe();
    if !powershell.is_file() {
        return Err("Nod could not find Windows PowerShell.".into());
    }
    let script = "$ErrorActionPreference='Stop'; $p=Start-Process -FilePath (Join-Path $env:SYSTEMROOT 'System32/wsl.exe') -ArgumentList '--install','--no-distribution','--web-download' -Verb RunAs -Wait -PassThru; exit $p.ExitCode";
    let mut command = Command::new(powershell);
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        script,
    ]);
    command.env("SYSTEMROOT", windows::system_root().to_string_lossy().to_string());
    windows::hide_console(&mut command);
    let output = command
        .output()
        .map_err(|error| format!("Windows could not open the WSL installer: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err("WSL installation did not finish. Restart Windows, then try again.".into())
    }
}

pub fn hold_command(name: &str) -> Vec<String> {
    vec![
        "--distribution".to_string(),
        name.to_string(),
        "--user".to_string(),
        "root".to_string(),
        "--exec".to_string(),
        "/usr/bin/sleep".to_string(),
        "infinity".to_string(),
    ]
}

#[allow(dead_code)]
pub fn is_unrepairable(value: &str) -> bool {
    is_unrepairable_wsl_code(value)
}

#[allow(dead_code)]
pub fn code(value: &str) -> Option<String> {
    parse_wsl_code(value)
}

#[allow(dead_code)]
pub fn tail(value: &str, limit: usize) -> String {
    let length = value.len().min(TAIL_LIMIT).min(limit);
    if value.len() <= length {
        value.to_string()
    } else {
        let mut start = value.len() - length;
        while start < value.len() && !value.is_char_boundary(start) {
            start += 1;
        }
        format!("…{}", &value[start..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_lists_are_decoded() {
        let value = "Ubuntu\r\nnod-e78f6754b82c\r\n";
        let mut bytes = vec![0xff, 0xfe];
        for unit in value.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let text = decode_windows_output(&bytes);
        assert!(text.contains("Ubuntu"));
        assert!(text.contains("nod-e78f6754b82c"));
        assert!(!text.contains('\0'));
    }

    #[test]
    fn lists_ignore_wsl_notices() {
        let value = "Windows Subsystem for Linux has no installed distributions.\n<none>\n* nod-a\r\n\n  nod-b \n";
        let values = parse_distributions(value);
        assert_eq!(values, vec!["nod-a", "nod-b"]);
    }

    #[test]
    fn conflicting_lists_are_rejected() {
        let registered = ["nod-a".to_string()];
        let running = ["nod-a".to_string(), "nod-b".to_string()];
        assert!(running.iter().any(|name| !registered.contains(name)));
    }
}
