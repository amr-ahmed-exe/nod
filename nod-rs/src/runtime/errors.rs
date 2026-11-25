use serde_json::{json, Value};

pub const NOD_WSL_REQUIRED: &str = "NOD_WSL_REQUIRED";
pub const RUNTIME_ARCHIVE_REQUIRED: &str = "RUNTIME_ARCHIVE_REQUIRED";
pub const NOD_RUNTIME_STOP_FAILED: &str = "NOD_RUNTIME_STOP_FAILED";
pub const NOD_COMPUTER_STORAGE_FULL: &str = "NOD_COMPUTER_STORAGE_FULL";
pub const NOD_COMPUTER_CONFIGURATION_INVALID: &str = "NOD_COMPUTER_CONFIGURATION_INVALID";
pub const NOD_COMPUTER_CONFIGURATION_MISSING: &str = "NOD_COMPUTER_CONFIGURATION_MISSING";
pub const NOD_COMPUTER_NOT_READY: &str = "NOD_COMPUTER_NOT_READY";
pub const NOD_COMPUTER_REGISTRATION_MISSING: &str = "NOD_COMPUTER_REGISTRATION_MISSING";
pub const NOD_WSL_COMMAND_FAILED: &str = "NOD_WSL_COMMAND_FAILED";
pub const NOD_WSL_IMPORT_FAILED: &str = "NOD_WSL_IMPORT_FAILED";
#[allow(dead_code)]
pub const NOD_CONTAINER_STOPPED: &str = "NOD_CONTAINER_STOPPED";
pub const NOD_COMPUTER_NETWORK_INVALID: &str = "NOD_COMPUTER_NETWORK_INVALID";
#[allow(dead_code)]
pub const NOD_COMPUTER_IDENTITY_CHANGED: &str = "NOD_COMPUTER_IDENTITY_CHANGED";
#[allow(dead_code)]
pub const NOD_RUNTIME_BUSY: &str = "NOD_RUNTIME_BUSY";
#[allow(dead_code)]
pub const NOD_RUNTIME_UNSUPPORTED: &str = "NOD_RUNTIME_UNSUPPORTED";

const MESSAGE_LIMIT: usize = 2_000;
const DETAILS_LIMIT: usize = 256 * 1024;

const UNREPAIRABLE_WSL_CODES: &[&str] = &[
    "ERROR_FILE_EXISTS",
    "ERROR_ALREADY_EXISTS",
    "E_ILLEGAL_STATE_CHANGE",
    "E_ACCESSDENIED",
    "ERROR_ACCESS_DENIED",
    "ERROR_DISK_FULL",
    "WSL_E_IMPORT_FAILED",
    "0x80072747",
    "0x800705b4",
];

const KNOWN_WSL_CODES: &[&str] = &[
    "HCS_E_SERVICE_NOT_AVAILABLE",
    "HCS_E_HYPERV_NOT_INSTALLED",
    "WSL_E_WSL_OPTIONAL_COMPONENT_REQUIRED",
    "ERROR_SERVICE_DISABLED",
    "ERROR_SERVICE_DOES_NOT_EXIST",
    "ERROR_FILE_EXISTS",
    "ERROR_ALREADY_EXISTS",
    "E_ILLEGAL_STATE_CHANGE",
    "E_ACCESSDENIED",
    "ERROR_ACCESS_DENIED",
    "ERROR_DISK_FULL",
    "WSL_E_IMPORT_FAILED",
    "0x80072747",
    "0x800705b4",
];

#[derive(Debug, Clone)]
pub struct RuntimeError {
    pub code: String,
    pub message: String,
    pub details: String,
    pub step: String,
    pub needs_wsl: bool,
    pub needs_runtime_archive: bool,
    pub needs_storage: bool,
    pub retryable: bool,
    pub exit_code: Option<i64>,
    pub wsl_code: Option<String>,
    pub restart_required: bool,
    pub actions: Vec<Value>,
}

impl RuntimeError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: bounded_message(message.into()),
            details: String::new(),
            step: "computer".to_string(),
            needs_wsl: false,
            needs_runtime_archive: false,
            needs_storage: false,
            retryable: true,
            exit_code: None,
            wsl_code: None,
            restart_required: false,
            actions: Vec::new(),
        }
    }

    pub fn step(mut self, step: &str) -> Self {
        self.step = step.to_string();
        self
    }

    pub fn details(mut self, details: impl AsRef<str>, redactor: &Redactor) -> Self {
        self.details = redactor.redact(details.as_ref(), DETAILS_LIMIT);
        self
    }

    pub fn exit_code(mut self, code: Option<i64>) -> Self {
        self.exit_code = code;
        self
    }

    pub fn wsl_code(mut self, code: Option<String>) -> Self {
        self.wsl_code = code;
        self
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn restart_required(mut self, required: bool) -> Self {
        self.restart_required = required;
        self
    }

    pub fn action(mut self, id: &str, label: &str) -> Self {
        self.actions.push(json!({ "id": id, "label": label }));
        self
    }

    #[allow(dead_code)]
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "code": self.code,
            "message": self.message,
            "step": self.step,
            "retryable": self.retryable,
            "needsWsl": self.needs_wsl,
            "needsRuntimeArchive": self.needs_runtime_archive,
            "needsStorage": self.needs_storage,
            "restartRequired": self.restart_required,
            "actions": self.actions
        });
        if !self.details.is_empty() {
            value["details"] = json!(self.details);
        }
        if let Some(code) = self.exit_code {
            value["exitCode"] = json!(code);
        }
        if let Some(code) = &self.wsl_code {
            value["wslCode"] = json!(code);
        }
        value
    }

    pub fn wsl_required() -> Self {
        Self::new(
            NOD_WSL_REQUIRED,
            "Windows Subsystem for Linux is not available. Install WSL, restart Windows, then open Nod again.",
        )
        .step("wsl")
        .needs_wsl()
    }

    pub fn wsl_broken(code: Option<String>, message: impl AsRef<str>, redactor: &Redactor) -> Self {
        let mut error = Self::new(
            NOD_WSL_REQUIRED,
            "WSL could not start Nod’s private computer. Restart Windows, then try again. If this repeats, install WSL from the setup screen.",
        )
        .step("wsl")
        .needs_wsl();
        error.wsl_code = code;
        error.details = redactor.redact(message.as_ref(), DETAILS_LIMIT);
        error
    }

    pub fn archive_required(reason: impl Into<String>) -> Self {
        let reason_text = reason.into();
        let message = if reason_text.trim().is_empty() {
            "Nod needs its Windows runtime file before it can create the private computer. Click Choose runtime file.".to_string()
        } else {
            format!(
                "{} Click Try again, or choose nod-Runtime-win32-x64.tar.gz with Choose runtime file.",
                reason_text.trim()
            )
        };
        Self::new(RUNTIME_ARCHIVE_REQUIRED, message)
            .step("assets")
            .needs_runtime_archive()
            .reason(reason_text)
            .action("import-runtime", "Choose runtime file")
    }

    pub fn storage_full(free: u64, needed: u64) -> Self {
        Self::new(
            NOD_COMPUTER_STORAGE_FULL,
            format!(
                "Nod needs about {} of free disk space for its private computer, but only {} is available. Free space, then choose Try again.",
                human_bytes(needed),
                human_bytes(free)
            ),
        )
        .step("storage")
        .needs_storage()
        .retryable(true)
    }

    pub fn configuration_invalid(detail: impl AsRef<str>, redactor: &Redactor) -> Self {
        let mut error = Self::new(
            NOD_COMPUTER_CONFIGURATION_INVALID,
            "The private computer configuration needs recovery. Its files were kept.",
        )
        .step("storage")
        .retryable(false)
        .action("recover", "Repair configuration");
        error.details = redactor.redact(detail.as_ref(), DETAILS_LIMIT);
        error
    }

    pub fn configuration_missing() -> Self {
        Self::new(
            NOD_COMPUTER_CONFIGURATION_MISSING,
            "Nod found a private computer without its saved configuration. Its files were kept.",
        )
        .step("storage")
        .retryable(false)
        .action("adopt", "Use existing computer")
        .action("import-runtime", "Restore configuration")
    }

    pub fn registration_missing() -> Self {
        Self::new(
            NOD_COMPUTER_REGISTRATION_MISSING,
            "Nod saved its private computer configuration, but WSL does not report the computer. Its files were kept.",
        )
        .step("wsl")
        .retryable(false)
        .action("reinstall", "Reinstall private computer")
        .action("import-runtime", "Choose runtime file")
    }

    pub fn import_failed(detail: impl AsRef<str>, redactor: &Redactor) -> Self {
        let mut error = Self::new(
            NOD_WSL_IMPORT_FAILED,
            "Nod could not import its private computer. The previous files were kept.",
        )
        .step("import")
        .needs_runtime_archive()
        .retryable(false)
        .action("import-runtime", "Choose runtime file")
        .action("retry", "Try again");
        error.details = redactor.redact(detail.as_ref(), DETAILS_LIMIT);
        error
    }

    pub fn not_ready(detail: impl AsRef<str>, redactor: &Redactor) -> Self {
        let mut error = Self::new(
            NOD_COMPUTER_NOT_READY,
            "Nod’s private computer started, but its workspace services did not become ready. Its files were kept.",
        )
        .step("health")
        .retryable(true)
        .action("retry", "Try again");
        error.details = redactor.redact(detail.as_ref(), DETAILS_LIMIT);
        error
    }

    pub fn stop_failed(detail: impl AsRef<str>, redactor: &Redactor) -> Self {
        let mut error = Self::new(
            NOD_RUNTIME_STOP_FAILED,
            "Nod could not confirm that its private computer stopped. Try again before quitting.",
        )
        .step("ready")
        .retryable(true);
        error.details = redactor.redact(detail.as_ref(), DETAILS_LIMIT);
        error
    }

    pub fn command_failed(
        step: &str,
        message: impl Into<String>,
        detail: impl AsRef<str>,
        exit_code: Option<i64>,
        redactor: &Redactor,
    ) -> Self {
        let mut error = Self::new(NOD_WSL_COMMAND_FAILED, message).step(step);
        error.exit_code = exit_code;
        error.details = redactor.redact(detail.as_ref(), DETAILS_LIMIT);
        error.wsl_code = parse_wsl_code(&error.details);
        error
    }

    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        if self.details.is_empty() {
            self.details = bounded_message(reason.into());
        } else {
            self.details.push('\n');
            self.details.push_str(&bounded_message(reason.into()));
        }
        self
    }

    pub fn needs_wsl(mut self) -> Self {
        self.needs_wsl = true;
        self
    }

    pub fn needs_runtime_archive(mut self) -> Self {
        self.needs_runtime_archive = true;
        self
    }

    pub fn needs_storage(mut self) -> Self {
        self.needs_storage = true;
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct Redactor {
    masks: Vec<(String, String)>,
}

impl Redactor {
    #[allow(dead_code)]
    pub fn new(mut values: Vec<(String, String)>) -> Self {
        values.retain(|(value, _)| !value.is_empty());
        values.sort_by_key(|left| std::cmp::Reverse(left.0.len()));
        values.dedup_by(|left, right| left.0 == right.0);
        Self { masks: values }
    }

    pub fn with_path(mut self, path: impl Into<String>, label: &str) -> Self {
        let path = path.into();
        if !path.is_empty() {
            let alternate = path.replace('\\', "/");
            self.masks.push((path, label.to_string()));
            if alternate != self.masks.last().map(|item| item.0.clone()).unwrap_or_default() {
                self.masks.push((alternate, label.to_string()));
            }
        }
        self.masks.sort_by_key(|left| std::cmp::Reverse(left.0.len()));
        self
    }

    pub fn secret(mut self, value: impl Into<String>) -> Self {
        let value = value.into();
        if value.len() >= 8 {
            self.masks.push((value, "[redacted]".to_string()));
        }
        self.masks.sort_by_key(|left| std::cmp::Reverse(left.0.len()));
        self
    }

    pub fn redact(&self, value: &str, limit: usize) -> String {
        let mut text = value.to_string();
        for (needle, replacement) in &self.masks {
            if !needle.is_empty() {
                text = text.replace(needle, replacement);
            }
        }
        text = mask_secret_patterns(&text);
        text = mask_control(&text);
        truncate(&text, limit.min(DETAILS_LIMIT))
    }

    pub fn message(&self, value: &str) -> String {
        self.redact(value, MESSAGE_LIMIT)
    }
}

fn truncate(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn mask_control(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character == '\n' || *character == '\t' || !character.is_control())
        .collect()
}

fn mask_secret_patterns(value: &str) -> String {
    let mut text = mask_sk_tokens(value);
    text = mask_bearer(&text);
    text = mask_desktop_token_header(&text);
    text = mask_postgres_urls(&text);
    text
}

fn mask_desktop_token_header(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.to_ascii_lowercase().find("x-nod-desktop-token:") {
        output.push_str(&rest[..index]);
        output.push_str("x-nod-desktop-token: [redacted]");
        rest = &rest[index + "x-nod-desktop-token:".len()..];
        let trimmed = rest.trim_start_matches([' ', '\t']);
        let end = trimmed
            .find(|ch: char| ch.is_whitespace() || ch == '"' || ch == '\'' || ch == ',')
            .unwrap_or(trimmed.len());
        rest = &trimmed[end..];
    }
    output.push_str(rest);
    output
}

fn mask_sk_tokens(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    loop {
        let Some(index) = rest.find("sk-") else {
            output.push_str(rest);
            break;
        };
        let boundary = rest[..index]
            .chars()
            .next_back()
            .map(|character| !character.is_ascii_alphanumeric())
            .unwrap_or(true);
        output.push_str(&rest[..index]);
        if !boundary {
            output.push_str("sk-");
            rest = &rest[index + 3..];
            continue;
        }
        output.push_str("sk-[redacted]");
        rest = &rest[index + 3..];
        let end = rest
            .find(|character: char| {
                !(character.is_ascii_alphanumeric() || character == '_' || character == '-')
            })
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    output
}

fn mask_bearer(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find("Bearer ") {
        output.push_str(&rest[..index]);
        output.push_str("Bearer [redacted]");
        rest = &rest[index + 7..];
        while let Some(character) = rest.chars().next() {
            if character.is_ascii_alphanumeric() || character == '.' || character == '_' || character == '-' {
                rest = &rest[character.len_utf8()..];
            } else {
                break;
            }
        }
    }
    output.push_str(rest);
    output
}

fn mask_postgres_urls(value: &str) -> String {
    let mut output = String::new();
    let mut rest = value;
    loop {
        let begin = match (rest.find("postgresql://"), rest.find("postgres://")) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(left), None) => Some(left),
            (None, Some(right)) => Some(right),
            (None, None) => None,
        };
        let Some(index) = begin else {
            output.push_str(rest);
            break;
        };
        output.push_str(&rest[..index]);
        output.push_str("postgresql://[redacted]");
        let after = &rest[index..];
        let end = after
            .find(|character: char| character.is_whitespace() || character == '"' || character == '\'')
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    output
}

pub fn parse_wsl_code(value: &str) -> Option<String> {
    if let Some(known) = KNOWN_WSL_CODES
        .iter()
        .find(|code| value.contains(**code))
        .map(|code| (*code).to_string())
    {
        return Some(known);
    }
    for segment in value.split_whitespace() {
        let trimmed = segment.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '/' && ch != '_');
        if let Some(rest) = trimmed.strip_prefix("Wsl/")
            && let Some(last) = rest.split('/').rfind(|part| !part.is_empty()) {
                let code: String = last
                    .chars()
                    .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                    .collect();
                if !code.is_empty() {
                    return Some(code);
                }
            }
    }
    None
}

pub fn is_missing_wsl_exit(code: Option<i32>) -> bool {
    code == Some(-1)
}

pub fn is_unrepairable_wsl_code(value: &str) -> bool {
    UNREPAIRABLE_WSL_CODES.iter().any(|code| value.contains(*code))
}

pub fn bounded_message(value: impl Into<String>) -> String {
    let value = value.into();
    let mut output = String::new();
    for character in value.chars() {
        if output.len() + character.len_utf8() > MESSAGE_LIMIT {
            break;
        }
        if character == '\n' || character == '\t' || !character.is_control() {
            output.push(character);
        }
    }
    output
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub fn is_safe_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 240
        && !value.starts_with('/')
        && !value.starts_with('\\')
        && !value.contains('\0')
        && !value.contains(':')
        && value
            .split(['/', '\\'])
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

#[allow(dead_code)]
pub fn is_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/'))
        || value.starts_with("\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_patterns_are_redacted() {
        let redactor = Redactor::new(vec![("C:\\secret\\root".into(), "<profile>".into())]);
        let value = redactor.redact(
            "failed C:\\secret\\root sk-abc123DEF456 Bearer tok_987654321 postgresql://nod:pw@127.0.0.1:5432/nod",
            4096,
        );
        assert!(!value.contains("secret"));
        assert!(!value.contains("abc123DEF456"));
        assert!(!value.contains("tok_987654321"));
        assert!(!value.contains("pw@"));
        assert!(value.contains("<profile>"));
    }

    #[test]
    fn messages_are_bounded() {
        let value = "x".repeat(50_000);
        assert!(bounded_message(value).chars().count() <= MESSAGE_LIMIT);
    }

    #[test]
    fn safe_paths_reject_traversal() {
        assert!(is_safe_relative_path("images/workspace.tar.zst"));
        assert!(!is_safe_relative_path("../guest.tar.gz"));
        assert!(!is_safe_relative_path("C:/guest.tar.gz"));
        assert!(!is_safe_relative_path("/guest.tar.gz"));
        assert!(!is_safe_relative_path("images//guest.tar.gz"));
    }

    #[test]
    fn wsl_codes_and_token_redaction_are_accurate() {
        assert_eq!(
            parse_wsl_code("Wsl/Service/RegisterDistro/HCS_E_SERVICE_NOT_AVAILABLE"),
            Some("HCS_E_SERVICE_NOT_AVAILABLE".to_string())
        );
        assert_eq!(
            parse_wsl_code("Wsl/Custom/Sub/E_CUSTOM_FAIL failed"),
            Some("E_CUSTOM_FAIL".to_string())
        );
        assert!(is_missing_wsl_exit(Some(-1)));
        assert!(!is_missing_wsl_exit(Some(0)));

        let redactor = Redactor::default();
        let cleaned = redactor.redact("header x-nod-desktop-token: abcdef1234567890 ok", 512);
        assert!(!cleaned.contains("abcdef1234567890"));
        assert!(cleaned.contains("x-nod-desktop-token: [redacted]"));

        let archive_err = RuntimeError::archive_required("Release feed timed out.");
        assert!(archive_err.needs_runtime_archive);
        assert!(archive_err.message.contains("Release feed timed out."));
    }
}
