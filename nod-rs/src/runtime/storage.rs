use crate::runtime::errors::{human_bytes, RuntimeError, NOD_COMPUTER_STORAGE_FULL};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SPACE_MARGIN: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub data_dir: PathBuf,
    pub runtime_root: PathBuf,
    pub control_root: PathBuf,
    pub computer_root: PathBuf,
    pub logs_root: PathBuf,
    pub settings_path: PathBuf,
    pub preferences_path: PathBuf,
    pub chat_read_cursors_path: PathBuf,
    pub state_path: PathBuf,
    pub startup_log_path: PathBuf,
    pub distribution: String,
}

impl RuntimePaths {
    pub fn discover() -> Self {
        let data_dir = normalize(&crate::db::data_directory());
        let runtime_root = normalize(&runtime_root_for(&data_dir));
        let control_root = runtime_root.join("desktop-runtime");
        let logs_root = runtime_root.join("logs");
        let distribution = distribution_name(&runtime_root);
        let state_path = control_root.join("state.json");
        let startup_log_path = logs_root.join("startup.log");
        let settings_path = runtime_root.join("settings.json");
        let preferences_path = runtime_root.join("preferences.json");
        let chat_read_cursors_path = runtime_root.join("chat-read-cursors.json");
        Self {
            data_dir,
            runtime_root,
            computer_root: control_root.join("wsl"),
            control_root,
            logs_root,
            settings_path,
            preferences_path,
            chat_read_cursors_path,
            state_path,
            startup_log_path,
            distribution,
        }
    }

    pub fn assets_root(&self) -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        for variable in ["NOD_RUNTIME_ASSETS", "NOD_WINDOWS_RUNTIME"] {
            if let Ok(value) = std::env::var(variable)
                && !value.trim().is_empty() {
                    candidates.push(PathBuf::from(value));
                }
        }
        if let Ok(executable) = std::env::current_exe()
            && let Some(directory) = executable.parent() {
                candidates.push(directory.join("windows-runtime"));
                candidates.push(directory.join("resources").join("engine").join("windows-runtime"));
                if let Some(parent) = directory.parent() {
                    candidates.push(parent.join("resources").join("engine").join("windows-runtime"));
                }
            }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            candidates.push(
                PathBuf::from(local)
                    .join("Programs")
                    .join("nod")
                    .join("resources")
                    .join("engine")
                    .join("windows-runtime"),
            );
        }
        candidates.push(self.control_root.join("runtime-assets"));
        candidates.push(self.data_dir.join("windows-runtime"));
        candidates.into_iter().find(|candidate| candidate.join("manifest.json").is_file())
    }

    pub fn import_assets_root(&self) -> PathBuf {
        self.control_root.join("runtime-assets")
    }

    pub fn ensure_roots(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.control_root)?;
        fs::create_dir_all(&self.logs_root)?;
        Ok(())
    }
}

pub fn runtime_root_for(data_dir: &Path) -> PathBuf {
    if let Ok(custom) = std::env::var("NOD_RUNTIME_ROOT")
        && !custom.trim().is_empty() {
            return PathBuf::from(custom);
        }
    let legacy = data_dir.join("profiles").join("local");
    if legacy.join("desktop-runtime").join("state.json").is_file()
        || legacy.join("desktop-runtime").join("wsl").is_dir()
    {
        legacy
    } else {
        data_dir.to_path_buf()
    }
}

pub fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize(path)
    } else {
        let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        normalize(&current.join(path))
    }
}

pub fn normalize(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    output.push(component.as_os_str());
                }
            }
            other => output.push(other.as_os_str()),
        }
    }
    if output.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        output
    }
}

pub fn distribution_name(root: &Path) -> String {
    let resolved = absolute(root);
    let mut value = resolved.to_string_lossy().to_string();
    if cfg!(windows) {
        value = value.replace('/', "\\");
    }
    while value.ends_with('\\') && value.len() > 3 {
        value.pop();
    }
    let digest = Sha256::digest(value.to_lowercase().as_bytes());
    let hash = hex(&digest);
    format!("nod-{}", &hash[..12])
}

pub fn identity_for(distribution: &str) -> String {
    distribution
        .strip_prefix("nod-")
        .unwrap_or(distribution)
        .to_string()
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    if getrandom::fill(&mut buffer).is_ok() {
        return hex(&buffer);
    }
    let seed = format!(
        "{}:{}:{}:{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0),
        std::thread::current().name().unwrap_or("nod"),
        std::time::Instant::now().elapsed().as_nanos()
    );
    hex(&Sha256::digest(seed.as_bytes()))
}

pub fn random_suffix(length: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut buffer = vec![0u8; length];
    if getrandom::fill(&mut buffer).is_err() {
        return random_hex(length).chars().take(length).collect();
    }
    buffer
        .iter()
        .map(|byte| ALPHABET[(*byte as usize) % ALPHABET.len()] as char)
        .collect()
}

pub fn available_bytes(path: &Path) -> Option<u64> {
    let existing = nearest_existing_ancestor(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = existing
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut available = 0u64;
        let mut total = 0u64;
        let mut total_free = 0u64;
        let success = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut available,
                &mut total,
                &mut total_free,
            )
        };
        if success != 0 {
            Some(available)
        } else {
            None
        }
    }
    #[cfg(not(windows))]
    {
        let _ = existing;
        None
    }
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut candidate = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    for _ in 0..32 {
        if candidate.exists() {
            return Some(candidate);
        }
        candidate = candidate.parent()?.to_path_buf();
    }
    None
}

pub fn check_space(path: &Path, required: u64) -> Result<u64, RuntimeError> {
    let Some(available) = available_bytes(path) else {
        return Ok(u64::MAX);
    };
    let needed = required.saturating_add(SPACE_MARGIN);
    if available < needed {
        return Err(RuntimeError::storage_full(available, needed));
    }
    Ok(available)
}

#[allow(dead_code)]
pub fn storage_summary(required: u64) -> String {
    format!("about {}", human_bytes(required.saturating_add(SPACE_MARGIN)))
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("state");
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".{name}.{}.tmp", random_suffix(12)));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    replace_file(&temporary, path)?;
    Ok(())
}

#[allow(dead_code)]
pub fn atomic_write_text(path: &Path, value: &str) -> std::io::Result<()> {
    atomic_write(path, value.as_bytes())
}

pub fn read_json(path: &Path) -> Option<serde_json::Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn write_json(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    atomic_write(path, &bytes)
}

#[cfg(windows)]
pub fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source_wide: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let target_wide: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result != 0 {
        return Ok(());
    }
    let _ = fs::remove_file(target);
    fs::rename(source, target)
}

#[cfg(not(windows))]
pub fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(source, target)
}

pub fn available_port() -> Result<u16, RuntimeError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|error| {
        RuntimeError::new(
            NOD_COMPUTER_STORAGE_FULL,
            format!("Nod could not allocate a local runtime port: {error}"),
        )
        .step("storage")
    })?;
    let port = listener
        .local_addr()
        .map_err(|error| {
            RuntimeError::new(
                NOD_COMPUTER_STORAGE_FULL,
                format!("Nod could not inspect its local runtime port: {error}"),
            )
            .step("storage")
        })?
        .port();
    Ok(port)
}

pub fn append_log_line(path: &Path, line: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "[{}] {}", now_iso(), line);
    }
}

pub fn sha256_file(path: &Path, mut progress: impl FnMut(u64, u64)) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let total = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut read = 0u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        read += count as u64;
        hasher.update(&buffer[..count]);
        progress(read, total);
    }
    Ok(hex(&hasher.finalize()))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn file_size(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|metadata| metadata.len())
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn legacy_distribution_name_is_stable() {
        let root = Path::new(r"C:\Users\Amr Ahmed\AppData\Roaming\nod\profiles\local");
        assert_eq!(distribution_name(root), "nod-e78f6754b82c");
    }

    #[test]
    fn relative_path_checks_are_strict() {
        assert_eq!(distribution_name(Path::new(".")).len(), 16);
        assert!(distribution_name(Path::new(".")).starts_with("nod-"));
    }

    #[test]
    fn atomic_write_replaces_content() {
        let directory = std::env::temp_dir().join(format!("nod-atomic-{}", random_suffix(10)));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("state.json");
        atomic_write(&path, b"one").unwrap();
        atomic_write(&path, b"two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn random_values_are_hex() {
        let value = random_hex(32);
        assert_eq!(value.len(), 64);
        assert!(value.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
