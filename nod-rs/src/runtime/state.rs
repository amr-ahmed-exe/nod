use crate::runtime::errors::{is_sha256, RuntimeError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    CheckingWsl,
    NeedsWsl,
    PreparingStorage,
    DownloadingRuntime,
    VerifyingRuntime,
    ImportingDistribution,
    StartingGuest,
    StartingServices,
    CheckingHealth,
    Ready,
    Stopping,
    Error,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::CheckingWsl => "checking_wsl",
            Self::NeedsWsl => "needs_wsl",
            Self::PreparingStorage => "preparing_storage",
            Self::DownloadingRuntime => "downloading_runtime",
            Self::VerifyingRuntime => "verifying_runtime",
            Self::ImportingDistribution => "importing_distribution",
            Self::StartingGuest => "starting_guest",
            Self::StartingServices => "starting_services",
            Self::CheckingHealth => "checking_health",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
            Self::Error => "error",
        }
    }

    pub fn top_phase(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::NeedsWsl | Self::Error => "error",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
            Self::CheckingWsl
            | Self::PreparingStorage
            | Self::DownloadingRuntime
            | Self::VerifyingRuntime
            | Self::ImportingDistribution => "preparing",
            Self::StartingGuest | Self::StartingServices | Self::CheckingHealth => "starting",
        }
    }

    pub fn ui_phase(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::NeedsWsl | Self::Error => "error",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
            Self::CheckingWsl | Self::PreparingStorage => "preparing",
            Self::DownloadingRuntime | Self::VerifyingRuntime => "downloading",
            Self::ImportingDistribution => "installing",
            Self::StartingGuest | Self::StartingServices | Self::CheckingHealth => "starting",
        }
    }

    pub fn stage(self) -> &'static str {
        match self {
            Self::Idle | Self::CheckingWsl | Self::NeedsWsl => "computer",
            Self::PreparingStorage
            | Self::DownloadingRuntime
            | Self::VerifyingRuntime
            | Self::ImportingDistribution => "storage",
            Self::StartingGuest | Self::StartingServices => "workspace",
            Self::CheckingHealth => "browser",
            Self::Ready => "ready",
            Self::Stopping | Self::Error => "computer",
        }
    }

    pub fn progress_start(self) -> Option<f64> {
        match self {
            Self::Idle => None,
            Self::CheckingWsl => Some(0.02),
            Self::NeedsWsl => None,
            Self::PreparingStorage => Some(0.08),
            Self::DownloadingRuntime => Some(0.16),
            Self::VerifyingRuntime => Some(0.34),
            Self::ImportingDistribution => Some(0.46),
            Self::StartingGuest => Some(0.66),
            Self::StartingServices => Some(0.76),
            Self::CheckingHealth => Some(0.92),
            Self::Ready => Some(1.0),
            Self::Stopping => None,
            Self::Error => None,
        }
    }

    #[allow(dead_code)]
    pub fn is_busy(self) -> bool {
        matches!(
            self,
            Self::CheckingWsl
                | Self::PreparingStorage
                | Self::DownloadingRuntime
                | Self::VerifyingRuntime
                | Self::ImportingDistribution
                | Self::StartingGuest
                | Self::StartingServices
                | Self::CheckingHealth
                | Self::Stopping
        )
    }

    #[allow(dead_code)]
    pub fn can_prepare(self) -> bool {
        !self.is_busy() && self != Self::Ready
    }
}

#[derive(Debug, Clone)]
pub struct Status {
    pub state: Phase,
    pub message: String,
    pub error: Option<String>,
    pub details: Option<String>,
    pub code: Option<String>,
    pub progress: Option<f64>,
    pub seconds_left: Option<u64>,
    pub needs_wsl: bool,
    pub needs_runtime_archive: bool,
    pub needs_storage: bool,
    pub restart_required: bool,
    pub distribution: Option<String>,
    pub health: Option<Value>,
    pub actions: Vec<Value>,
    pub updated_at: String,
}

impl Status {
    pub fn idle(message: impl Into<String>, distribution: Option<String>) -> Self {
        Self {
            state: Phase::Idle,
            message: message.into(),
            error: None,
            details: None,
            code: None,
            progress: None,
            seconds_left: None,
            needs_wsl: false,
            needs_runtime_archive: false,
            needs_storage: false,
            restart_required: false,
            distribution,
            health: None,
            actions: Vec::new(),
            updated_at: now(),
        }
    }

    pub fn transition(&mut self, state: Phase, message: impl Into<String>, progress: Option<f64>) {
        self.state = state;
        self.message = message.into();
        self.error = None;
        self.details = None;
        self.code = None;
        self.progress = progress;
        self.seconds_left = None;
        self.needs_wsl = false;
        self.needs_runtime_archive = false;
        self.needs_storage = false;
        self.restart_required = false;
        self.actions.clear();
        self.updated_at = now();
    }

    pub fn fail(&mut self, failure: &RuntimeError) {
        self.state = if failure.needs_wsl {
            Phase::NeedsWsl
        } else {
            Phase::Error
        };
        self.message = failure.message.clone();
        self.error = Some(failure.message.clone());
        self.details = if failure.details.is_empty() {
            None
        } else {
            Some(failure.details.clone())
        };
        self.code = Some(failure.code.clone());
        self.progress = None;
        self.seconds_left = None;
        self.needs_wsl = failure.needs_wsl;
        self.needs_runtime_archive = failure.needs_runtime_archive;
        self.needs_storage = failure.needs_storage;
        self.restart_required = failure.restart_required;
        self.actions = failure.actions.clone();
        self.updated_at = now();
    }

    pub fn to_json(&self) -> Value {
        let runtime_obj = json!({
            "phase": self.state.ui_phase(),
            "state": self.state.as_str(),
            "stage": self.state.stage(),
            "message": self.message,
            "error": self.error,
            "details": self.details,
            "code": self.code,
            "progress": self.progress,
            "secondsLeft": self.seconds_left,
            "needsWsl": self.needs_wsl,
            "needsRuntimeArchive": self.needs_runtime_archive,
            "needsStorage": self.needs_storage,
            "restartRequired": self.restart_required,
            "needsServerConnection": false,
            "engineReady": self.state == Phase::Ready,
            "distribution": self.distribution,
            "health": self.health,
            "actions": self.actions,
            "updatedAt": self.updated_at
        });
        json!({
            "platform": "win32",
            "remote": false,
            "unlockingKeys": false,
            "phase": self.state.top_phase(),
            "state": self.state.as_str(),
            "stage": self.state.stage(),
            "message": self.message,
            "error": self.error,
            "details": self.details,
            "code": self.code,
            "progress": self.progress,
            "secondsLeft": self.seconds_left,
            "needsWsl": self.needs_wsl,
            "needsRuntimeArchive": self.needs_runtime_archive,
            "needsStorage": self.needs_storage,
            "restartRequired": self.restart_required,
            "needsServerConnection": false,
            "engineReady": self.state == Phase::Ready,
            "distribution": self.distribution,
            "health": self.health,
            "actions": self.actions,
            "updatedAt": self.updated_at,
            "runtime": runtime_obj
        })
    }
}

fn now() -> String {
    crate::util::iso_now()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedState {
    pub version: u32,
    pub distribution: String,
    pub password: String,
    pub guest: String,
    #[serde(default)]
    pub initializing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    pub services_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<String>,
}

impl PersistedState {
    pub fn new(distribution: String, guest_hash: String) -> Self {
        Self {
            version: 1,
            distribution,
            password: crate::runtime::storage::random_hex(32),
            guest: guest_hash,
            initializing: true,
            computer_path: None,
            holder_pid: None,
            port: None,
            services_ready: false,
            ready_at: None,
        }
    }

    pub fn validate(&self, expected_distribution: &str) -> Result<(), String> {
        if self.version != 1 {
            return Err("The saved runtime version is not supported.".into());
        }
        if self.distribution != expected_distribution {
            return Err("The saved runtime belongs to a different private computer.".into());
        }
        if !is_sha256(&self.password) {
            return Err("The saved private computer password is invalid.".into());
        }
        if !is_sha256(&self.guest) {
            return Err("The saved runtime image hash is invalid.".into());
        }
        if self.services_ready && self.initializing {
            return Err("The saved runtime claims to be ready while it is still initializing.".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupPlan {
    Fresh,
    ResumeImport,
    StartExisting,
    AlreadyReady,
}

pub fn classify(
    saved: Option<&PersistedState>,
    registered: bool,
    running: bool,
    expected_distribution: &str,
) -> Result<SetupPlan, RuntimeError> {
    match (saved, registered) {
        (None, true) => Err(RuntimeError::configuration_missing()),
        (None, false) => Ok(SetupPlan::Fresh),
        (Some(state), false) => {
            if state.initializing {
                Ok(SetupPlan::ResumeImport)
            } else {
                let _ = state.validate(expected_distribution);
                Err(RuntimeError::registration_missing())
            }
        }
        (Some(state), true) => {
            state
                .validate(expected_distribution)
                .map_err(|detail| RuntimeError::new(
                    crate::runtime::errors::NOD_COMPUTER_CONFIGURATION_INVALID,
                    "The private computer configuration needs recovery. Its files were kept.",
                ).step("storage").details(detail, &crate::runtime::errors::Redactor::default()))?;
            if state.services_ready && running && !state.initializing {
                Ok(SetupPlan::AlreadyReady)
            } else if state.initializing {
                Ok(SetupPlan::ResumeImport)
            } else {
                Ok(SetupPlan::StartExisting)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PersistedState {
        PersistedState {
            version: 1,
            distribution: "nod-123456789abc".into(),
            password: "a".repeat(64),
            guest: "b".repeat(64),
            initializing: false,
            computer_path: Some(r"C:\Nod\wsl".into()),
            holder_pid: None,
            port: Some(54329),
            services_ready: true,
            ready_at: None,
        }
    }

    #[test]
    fn classification_keeps_existing_data_without_guessing_ready() {
        let saved = state();
        assert_eq!(
            classify(Some(&saved), true, false, "nod-123456789abc").unwrap(),
            SetupPlan::StartExisting
        );
        assert_eq!(
            classify(None, true, false, "nod-123456789abc").unwrap_err().code,
            crate::runtime::errors::NOD_COMPUTER_CONFIGURATION_MISSING
        );
        assert_eq!(
            classify(Some(&saved), false, false, "nod-123456789abc").unwrap_err().code,
            crate::runtime::errors::NOD_COMPUTER_REGISTRATION_MISSING
        );
        assert_eq!(
            classify(Some(&saved), true, true, "nod-123456789abc").unwrap(),
            SetupPlan::AlreadyReady
        );
    }

    #[test]
    fn interrupted_import_is_resumed_instead_of_deleted() {
        let mut saved = state();
        saved.initializing = true;
        saved.services_ready = false;
        assert_eq!(
            classify(Some(&saved), true, false, "nod-123456789abc").unwrap(),
            SetupPlan::ResumeImport
        );
    }

    #[test]
    fn status_never_claims_engine_ready_before_health() {
        let mut status = Status::idle("Starting", None);
        status.transition(Phase::StartingGuest, "Starting guest", Some(0.7));
        assert!(!status.to_json()["engineReady"].as_bool().unwrap());
        status.transition(Phase::Ready, "Ready", Some(1.0));
        assert!(status.to_json()["engineReady"].as_bool().unwrap());
    }
}
