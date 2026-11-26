use crate::runtime::errors::{
    is_safe_relative_path, is_sha256, Redactor, RuntimeError, NOD_WSL_COMMAND_FAILED,
    RUNTIME_ARCHIVE_REQUIRED,
};
use crate::runtime::state::PersistedState;
use crate::runtime::storage::{self, RuntimePaths};
use crate::runtime::windows;
use crate::runtime::wsl::Wsl;
use flate2::read::GzDecoder;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const GIB: u64 = 1024 * 1024 * 1024;
const MAX_GUEST_BYTES: u64 = 8 * GIB;
const MAX_EXPANDED_BYTES: u64 = 12 * GIB;
const MAX_IMAGE_BYTES: u64 = 24 * GIB;
const STAGING_MARGIN: u64 = 2 * GIB;

#[derive(Debug, Clone)]
pub struct GuestAsset {
    pub file: String,
    pub bytes: u64,
    pub expanded_bytes: u64,
    pub sha256: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImageAsset {
    pub name: String,
    pub reference: String,
    pub id: String,
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
    #[allow(dead_code)]
    pub url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TransportAsset {
    pub file: String,
    pub sha256: Option<String>,
    #[allow(dead_code)]
    pub url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RuntimeManifest {
    pub root: PathBuf,
    #[allow(dead_code)]
    pub version: u32,
    #[allow(dead_code)]
    pub platform: String,
    pub guest: GuestAsset,
    pub images: Vec<ImageAsset>,
    pub transport: TransportAsset,
    pub release_tag: Option<String>,
    pub base_url: Option<String>,
}

impl RuntimeManifest {
    pub fn guest_path(&self) -> PathBuf {
        self.root.join(&self.guest.file)
    }

    pub fn transport_path(&self) -> PathBuf {
        self.root.join(&self.transport.file)
    }

    pub fn uses_wsl_transport(&self) -> bool {
        self.transport.sha256.is_none() || self.transport.file == "wsl-direct"
    }

    pub fn image_path(&self, image: &ImageAsset) -> PathBuf {
        self.root.join(&image.file)
    }

    pub fn image(&self, name: &str) -> Option<&ImageAsset> {
        self.images.iter().find(|image| image.name == name)
    }

    pub fn guest_download_url(&self) -> Option<String> {
        if let Some(url) = &self.guest.url {
            return Some(url.clone());
        }
        for variable in ["NOD_RUNTIME_URL", "NOD_RUNTIME_DOWNLOAD_URL"] {
            if let Ok(value) = std::env::var(variable)
                && !value.trim().is_empty() {
                    return Some(value);
                }
        }
        if let Some(base) = self.base_url.as_deref() {
            return Some(format!("{}/{}", base.trim_end_matches('/'), self.guest.file));
        }
        if let Some(tag) = self.release_tag.as_deref() {
            let trimmed = tag.trim();
            if !trimmed.is_empty()
                && trimmed
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
            {
                let repo = std::env::var("NOD_UPDATE_REPO")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "amr-ahmed-exe/nod".to_string());
                return Some(format!(
                    "https://github.com/{repo}/releases/download/{trimmed}/{}",
                    self.guest.file
                ));
            }
        }
        None
    }

    #[allow(dead_code)]
    pub fn staged_bytes(&self) -> u64 {
        self.guest
            .expanded_bytes
            .saturating_add(self.guest.bytes)
            .saturating_add(
                self.images
                    .iter()
                    .fold(0u64, |total, image| total.saturating_add(image.bytes)),
            )
            .saturating_add(STAGING_MARGIN)
    }

    pub fn installed_bytes(&self) -> u64 {
        self.guest
            .expanded_bytes
            .saturating_add(
                self.images
                    .iter()
                    .fold(0u64, |total, image| total.saturating_add(image.bytes)),
            )
            .saturating_add(STAGING_MARGIN)
    }
}

pub fn load_manifest(path: &Path) -> Result<RuntimeManifest, RuntimeError> {
    let bytes = fs::read(path).map_err(|error| {
        RuntimeError::archive_required(format!(
            "The runtime manifest at {} could not be read: {error}",
            path.display()
        ))
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        RuntimeError::archive_required("The Windows runtime manifest is invalid.")
            .reason(format!("{error}"))
    })?;
    let root = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    if value.get("transportMode").and_then(Value::as_str) == Some("wsl") {
        let saved_guest = value
            .get("guest")
            .and_then(|g| g.get("sha256"))
            .or_else(|| value.get("guestSha256"))
            .and_then(Value::as_str);
        return parse_guest_manifest(&value, &root, saved_guest);
    }
    parse_manifest(&value, &root)
}

pub fn find_manifest(paths: &RuntimePaths) -> Result<Option<RuntimeManifest>, RuntimeError> {
    if let Ok(value) = std::env::var("NOD_RUNTIME_MANIFEST")
        && !value.trim().is_empty() {
            return load_manifest(Path::new(&value)).map(Some);
        }
    if let Ok(value) = std::env::var("NOD_RUNTIME_ASSETS")
        && !value.trim().is_empty() {
            let path = Path::new(&value).join("manifest.json");
            if path.is_file() {
                return load_manifest(&path).map(Some);
            }
        }
    let Some(root) = paths.assets_root() else {
        return Ok(None);
    };
    let path = root.join("manifest.json");
    if !path.is_file() {
        return Ok(None);
    }
    load_manifest(&path).map(Some)
}

fn parse_images(object: &serde_json::Map<String, Value>) -> Result<Vec<ImageAsset>, RuntimeError> {
    let mut images = Vec::new();
    let values = object
        .get("images")
        .and_then(Value::as_array)
        .ok_or_else(|| RuntimeError::archive_required("The Windows runtime containers are missing."))?;
    if values.len() != 3 {
        return Err(RuntimeError::archive_required(
            "The Windows runtime must contain the workspace, browser and postgres containers.",
        ));
    }
    for value in values {
        let image = value.as_object().ok_or_else(|| {
            RuntimeError::archive_required("A Windows runtime container entry is invalid.")
        })?;
        let name = string(image, "name")?;
        if !matches!(name.as_str(), "workspace" | "browser" | "postgres") {
            return Err(RuntimeError::archive_required(
                "The Windows runtime contains an unknown container.",
            ));
        }
        if images.iter().any(|current: &ImageAsset| current.name == name) {
            return Err(RuntimeError::archive_required(
                "The Windows runtime repeats a container entry.",
            ));
        }
        let reference = string(image, "reference").unwrap_or_else(|_| name.clone());
        let id = string(image, "id")?;
        let Some(digest_value) = id.strip_prefix("sha256:") else {
            return Err(RuntimeError::archive_required(
                "A Windows runtime container image identifier is invalid.",
            ));
        };
        if !is_sha256(digest_value) {
            return Err(RuntimeError::archive_required(
                "A Windows runtime container image identifier is invalid.",
            ));
        }
        let file = image
            .get("file")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("images/{name}.tar.zst"));
        if !is_safe_relative_path(&file) {
            return Err(RuntimeError::archive_required(
                "A Windows runtime container file path is invalid.",
            ));
        }
        let bytes = image
            .get("size")
            .and_then(Value::as_u64)
            .or_else(|| image.get("bytes").and_then(Value::as_u64))
            .unwrap_or(0);
        if bytes == 0 || bytes > MAX_IMAGE_BYTES {
            return Err(RuntimeError::archive_required(
                "A Windows runtime container size is invalid.",
            ));
        }
        let sha256 = image
            .get("sha256")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !sha256.is_empty() && !is_sha256(&sha256) {
            return Err(RuntimeError::archive_required(
                "A Windows runtime container hash is invalid.",
            ));
        }
        images.push(ImageAsset {
            name,
            reference,
            id,
            file,
            bytes,
            sha256,
            url: image.get("url").and_then(Value::as_str).map(str::to_string),
        });
    }
    for required in ["workspace", "browser", "postgres"] {
        if !images.iter().any(|image| image.name == required) {
            return Err(RuntimeError::archive_required(format!(
                "The Windows runtime is missing the {required} container."
            )));
        }
    }
    Ok(images)
}

pub fn parse_manifest(value: &Value, root: &Path) -> Result<RuntimeManifest, RuntimeError> {
    let object = value.as_object().ok_or_else(|| {
        RuntimeError::archive_required("The Windows runtime manifest is not an object.")
    })?;
    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| RuntimeError::archive_required("The Windows runtime version is missing."))?;
    if version != 1 {
        return Err(RuntimeError::archive_required(
            "This Windows runtime version is not supported.",
        ));
    }
    let platform = object
        .get("platform")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if platform != "win32-x64" {
        return Err(RuntimeError::archive_required(
            "This Windows runtime was built for a different platform.",
        ));
    }

    let guest_value = object
        .get("guest")
        .and_then(Value::as_object)
        .ok_or_else(|| RuntimeError::archive_required("The Windows runtime image is missing."))?;
    let guest_file = guest_value
        .get("file")
        .and_then(Value::as_str)
        .unwrap_or("guest.tar.gz");
    if guest_file != "guest.tar.gz" || !is_safe_relative_path(guest_file) {
        return Err(RuntimeError::archive_required(
            "The Windows runtime image name is invalid.",
        ));
    }
    let guest_bytes = number(guest_value, "bytes")?;
    let expanded_bytes = number(guest_value, "expandedBytes")?;
    let guest_sha = digest(guest_value, "sha256")?;
    if guest_bytes == 0
        || guest_bytes > MAX_GUEST_BYTES
        || expanded_bytes < guest_bytes
        || expanded_bytes > MAX_EXPANDED_BYTES
    {
        return Err(RuntimeError::archive_required(
            "The Windows runtime image size is invalid.",
        ));
    }

    let images = parse_images(object)?;

    let transport_value = object
        .get("transport")
        .and_then(Value::as_object)
        .ok_or_else(|| RuntimeError::archive_required("The Windows runtime transport is missing."))?;
    let transport_file = string(transport_value, "file")?;
    if transport_file != "bin/docker.exe" || !is_safe_relative_path(&transport_file) {
        return Err(RuntimeError::archive_required(
            "The Windows runtime transport path is invalid.",
        ));
    }
    let transport = TransportAsset {
        file: transport_file,
        sha256: Some(digest(transport_value, "sha256")?),
        url: transport_value
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string),
    };

    let base_url = object
        .get("baseUrl")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    if let Some(url) = &base_url {
        validate_url(url)?;
    }
    let guest_url = guest_value
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(url) = &guest_url {
        validate_url(url)?;
    }

    Ok(RuntimeManifest {
        root: root.to_path_buf(),
        version: version as u32,
        platform: platform.to_string(),
        guest: GuestAsset {
            file: guest_file.to_string(),
            bytes: guest_bytes,
            expanded_bytes,
            sha256: guest_sha,
            url: guest_url,
        },
        images,
        transport,
        release_tag: object
            .get("releaseTag")
            .and_then(Value::as_str)
            .map(str::to_string),
        base_url,
    })
}

/// Parses a manifest recovered from inside WSL `/opt/nod-assets/manifest.json` or standalone Windows release metadata.
pub fn parse_guest_manifest(
    value: &Value,
    root: &Path,
    saved_guest_sha256: Option<&str>,
) -> Result<RuntimeManifest, RuntimeError> {
    let object = value.as_object().ok_or_else(|| {
        RuntimeError::archive_required("The Windows runtime manifest is not an object.")
    })?;
    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| RuntimeError::archive_required("The Windows runtime version is missing."))?;
    if version != 1 {
        return Err(RuntimeError::archive_required(
            "This Windows runtime version is not supported.",
        ));
    }
    let platform = object
        .get("platform")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if platform != "win32-x64" {
        return Err(RuntimeError::archive_required(
            "This Windows runtime was built for a different platform.",
        ));
    }
    let images = parse_images(object)?;
    let wsl_recovered_empty_guest = object.get("transportMode").and_then(Value::as_str) == Some("wsl")
        && object
            .get("guest")
            .and_then(Value::as_object)
            .map(|g| {
                g.get("bytes").and_then(Value::as_u64).unwrap_or(0) == 0
                    && g.get("expandedBytes").and_then(Value::as_u64).unwrap_or(0) == 0
                    && g.get("url").and_then(Value::as_str).is_none()
            })
            .unwrap_or(false);
    let guest = if let Some(guest_value) = object.get("guest").and_then(Value::as_object)
        && !wsl_recovered_empty_guest
    {
        let guest_file = guest_value
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or("guest.tar.gz");
        if guest_file != "guest.tar.gz" || !is_safe_relative_path(guest_file) {
            return Err(RuntimeError::archive_required(
                "The Windows runtime image name is invalid.",
            ));
        }
        let guest_bytes = number(guest_value, "bytes")?;
        let expanded_bytes = number(guest_value, "expandedBytes")?;
        let guest_sha = digest(guest_value, "sha256")?;
        if guest_bytes == 0
            || guest_bytes > MAX_GUEST_BYTES
            || expanded_bytes < guest_bytes
            || expanded_bytes > MAX_EXPANDED_BYTES
        {
            return Err(RuntimeError::archive_required(
                "The Windows runtime image size is invalid.",
            ));
        }
        let guest_url = guest_value
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(url) = &guest_url {
            validate_url(url)?;
        }
        GuestAsset {
            file: guest_file.to_string(),
            bytes: guest_bytes,
            expanded_bytes,
            sha256: guest_sha,
            url: guest_url,
        }
    } else {
        let sha = saved_guest_sha256
            .filter(|v| is_sha256(v))
            .unwrap_or("0000000000000000000000000000000000000000000000000000000000000000");
        GuestAsset {
            file: "guest.tar.gz".to_string(),
            bytes: 0,
            expanded_bytes: 0,
            sha256: sha.to_string(),
            url: None,
        }
    };

    let transport = if let Some(transport_value) = object.get("transport").and_then(Value::as_object) {
        let transport_file = string(transport_value, "file")?;
        if transport_file == "wsl-direct" {
            TransportAsset {
                file: transport_file,
                sha256: None,
                url: None,
            }
        } else {
            if transport_file != "bin/docker.exe" || !is_safe_relative_path(&transport_file) {
                return Err(RuntimeError::archive_required(
                    "The Windows runtime transport path is invalid.",
                ));
            }
            TransportAsset {
                file: transport_file,
                sha256: Some(digest(transport_value, "sha256")?),
                url: transport_value
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }
        }
    } else {
        TransportAsset {
            file: "wsl-direct".to_string(),
            sha256: None,
            url: None,
        }
    };

    let base_url = object
        .get("baseUrl")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    if let Some(url) = &base_url {
        validate_url(url)?;
    }

    Ok(RuntimeManifest {
        root: root.to_path_buf(),
        version: version as u32,
        platform: platform.to_string(),
        guest,
        images,
        transport,
        release_tag: object
            .get("releaseTag")
            .and_then(Value::as_str)
            .map(str::to_string),
        base_url,
    })
}

/// Recovers `manifest.json` from inside an existing WSL distribution when the Electron host files were removed.
pub fn recover_manifest_from_wsl(
    wsl: &Wsl,
    paths: &RuntimePaths,
    saved: Option<&PersistedState>,
    redactor: &Redactor,
) -> Result<RuntimeManifest, RuntimeError> {
    let out = wsl
        .guest(
            &paths.distribution,
            &["/bin/cat", "/opt/nod-assets/manifest.json"],
            None,
            Duration::from_secs(20),
        )
        .map_err(|err| {
            RuntimeError::archive_required(
                "Could not read the runtime manifest from the existing private computer.",
            )
            .details(err, redactor)
        })?;
    if !out.success() {
        return Err(RuntimeError::archive_required(
            "The Windows runtime manifest could not be found on this computer or inside WSL.",
        )
        .details(out.combined(), redactor));
    }
    let check_docker = wsl
        .guest(
            &paths.distribution,
            &["test", "-x", "/usr/local/bin/nod-docker"],
            None,
            Duration::from_secs(15),
        )
        .map(|o| o.success())
        .unwrap_or(false);
    if !check_docker {
        return Err(RuntimeError::archive_required(
            "The existing private computer is missing /usr/local/bin/nod-docker. Choose its runtime file to restore it.",
        ));
    }

    let mut raw: Value = serde_json::from_str(out.stdout.trim()).map_err(|error| {
        RuntimeError::archive_required("The guest runtime manifest is invalid.")
            .reason(format!("{error}"))
    })?;
    let target_root = paths.import_assets_root();
    let manifest = parse_guest_manifest(
        &raw,
        &target_root,
        saved.map(|s| s.guest.as_str()),
    )?;
    if let Some(obj) = raw.as_object_mut() {
        obj.insert("transportMode".to_string(), Value::String("wsl".to_string()));
        if !obj.contains_key("guest") {
            obj.insert(
                "guestSha256".to_string(),
                Value::String(manifest.guest.sha256.clone()),
            );
        }
    }
    let _ = fs::create_dir_all(&target_root);
    let persisted_bytes = serde_json::to_vec_pretty(&raw).map_err(|error| {
        RuntimeError::archive_required(format!("Could not serialize recovered manifest: {error}"))
    })?;
    storage::atomic_write(&target_root.join("manifest.json"), &persisted_bytes).map_err(|error| {
        RuntimeError::archive_required(format!("Could not cache recovered manifest: {error}"))
    })?;
    Ok(manifest)
}

fn select_release_asset<'a>(assets: &'a [Value], prefix: &str, exact: bool) -> Option<&'a Value> {
    assets
        .iter()
        .filter(|entry| {
            let matches = entry.get("name").and_then(Value::as_str).is_some_and(|name| {
                if exact {
                    name == prefix
                } else {
                    name.starts_with(prefix) && name.ends_with(".json")
                }
            });
            matches && entry.get("state").and_then(Value::as_str) != Some("draft")
        })
        .max_by_key(|entry| {
            entry
                .get("created_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
        })
}

pub fn fetch_latest_windows_runtime_manifest(
    paths: &RuntimePaths,
    _redactor: &Redactor,
) -> Result<RuntimeManifest, RuntimeError> {
    let default_repo = std::env::var("NOD_UPDATE_REPO")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "amr-ahmed-exe/nod".to_string());
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(30))
        .user_agent("Nod/0.1 native")
        .build();
    let target_root = paths.import_assets_root();

    let api_url = std::env::var("NOD_DESKTOP_RELEASE_API")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("https://api.github.com/repos/{default_repo}/releases/latest"));
    validate_url(&api_url)?;

    if let Ok(release_resp) = agent
        .get(&api_url)
        .set("Accept", "application/vnd.github+json")
        .call()
        && let Ok(release_json) = serde_json::from_reader::<_, Value>(release_resp.into_reader())
        && let Some(tag) = release_json
            .get("tag_name")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
        && let Some(assets_arr) = release_json.get("assets").and_then(Value::as_array)
    {
        let find_asset = |prefix: &str, exact: bool| -> Option<(String, u64, Option<String>)> {
            select_release_asset(assets_arr, prefix, exact).and_then(|entry| {
                let url = entry.get("browser_download_url").and_then(Value::as_str)?.to_string();
                let size = entry.get("size").and_then(Value::as_u64)?;
                let sha = entry
                    .get("digest")
                    .and_then(Value::as_str)
                    .and_then(|d| d.strip_prefix("sha256:"))
                    .map(str::to_string);
                Some((url, size, sha))
            })
        };

        if let Some((guest_url, guest_size, guest_digest)) =
            find_asset("nod-Runtime-win32-x64.tar.gz", true)
            && let Some((runtime_meta_url, runtime_meta_size, _)) =
                find_asset("runtime-win32-x64-", false)
                    .or_else(|| find_asset("runtime-win32-x64.json", true))
        {
            validate_url(&guest_url)?;
            validate_url(&runtime_meta_url)?;
            if runtime_meta_size > 2 * 1024 * 1024 {
                return Err(RuntimeError::archive_required(
                    "The Windows runtime descriptor is too large.",
                ));
            }
            let meta_resp = agent.get(&runtime_meta_url).call().map_err(|err| {
                RuntimeError::archive_required(format!(
                    "Could not download Windows runtime descriptor: {err}"
                ))
            })?;
            let mut raw: Value = serde_json::from_reader(meta_resp.into_reader()).map_err(|err| {
                RuntimeError::archive_required(format!("Invalid Windows runtime descriptor: {err}"))
            })?;
            if let Some(obj) = raw.as_object_mut() {
                obj.insert("transportMode".to_string(), Value::String("wsl".to_string()));
                obj.insert("releaseTag".to_string(), Value::String(tag.to_string()));
                if let Some(guest_obj) = obj.get_mut("guest").and_then(Value::as_object_mut) {
                    if let Some(expected_sha) = &guest_digest
                        && guest_obj.get("sha256").and_then(Value::as_str)
                            != Some(expected_sha.as_str())
                    {
                        return Err(RuntimeError::archive_required(
                            "The release asset checksum does not match the runtime manifest.",
                        ));
                    }
                    if guest_obj.get("bytes").and_then(Value::as_u64) != Some(guest_size) {
                        return Err(RuntimeError::archive_required(
                            "The release asset size does not match the runtime manifest.",
                        ));
                    }
                    guest_obj.insert("url".to_string(), Value::String(guest_url));
                }
            }
            let manifest = parse_guest_manifest(&raw, &target_root, None)?;
            let _ = fs::create_dir_all(&target_root);
            let persisted = serde_json::to_vec_pretty(&raw).map_err(|err| {
                RuntimeError::archive_required(format!("Could not store runtime manifest: {err}"))
            })?;
            storage::atomic_write(&target_root.join("manifest.json"), &persisted).map_err(|err| {
                RuntimeError::archive_required(format!("Could not write runtime manifest: {err}"))
            })?;
            return Ok(manifest);
        }
    }

    let direct_meta_url =
        format!("https://github.com/{default_repo}/releases/latest/download/runtime-win32-x64.json");
    let direct_guest_url =
        format!("https://github.com/{default_repo}/releases/latest/download/nod-Runtime-win32-x64.tar.gz");
    validate_url(&direct_meta_url)?;
    let meta_resp = agent.get(&direct_meta_url).call().map_err(|err| {
        RuntimeError::archive_required(format!(
            "Could not fetch the Alpine WSL2 runtime from GitHub Releases: {err}"
        ))
    })?;
    let mut raw: Value = serde_json::from_reader(meta_resp.into_reader()).map_err(|err| {
        RuntimeError::archive_required(format!("Invalid Windows runtime descriptor: {err}"))
    })?;
    if let Some(obj) = raw.as_object_mut() {
        obj.insert("transportMode".to_string(), Value::String("wsl".to_string()));
        if let Some(guest_obj) = obj.get_mut("guest").and_then(Value::as_object_mut)
            && guest_obj.get("url").and_then(Value::as_str).is_none()
        {
            guest_obj.insert("url".to_string(), Value::String(direct_guest_url));
        }
    }
    let manifest = parse_guest_manifest(&raw, &target_root, None)?;
    let _ = fs::create_dir_all(&target_root);
    let persisted = serde_json::to_vec_pretty(&raw).map_err(|err| {
        RuntimeError::archive_required(format!("Could not store runtime manifest: {err}"))
    })?;
    storage::atomic_write(&target_root.join("manifest.json"), &persisted).map_err(|err| {
        RuntimeError::archive_required(format!("Could not write runtime manifest: {err}"))
    })?;
    Ok(manifest)
}

fn number(object: &serde_json::Map<String, Value>, key: &str) -> Result<u64, RuntimeError> {
    object.get(key).and_then(Value::as_u64).ok_or_else(|| {
        RuntimeError::archive_required(format!("The Windows runtime {key} is invalid."))
    })
}

fn string(object: &serde_json::Map<String, Value>, key: &str) -> Result<String, RuntimeError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            RuntimeError::archive_required(format!("The Windows runtime {key} is missing."))
        })
}

fn digest(object: &serde_json::Map<String, Value>, key: &str) -> Result<String, RuntimeError> {
    let value = string(object, key)?;
    if is_sha256(&value) {
        Ok(value)
    } else {
        Err(RuntimeError::archive_required(format!(
            "The Windows runtime {key} is invalid."
        )))
    }
}

#[allow(dead_code)]
pub fn verify_url(value: &str) -> Result<(), RuntimeError> {
    validate_url(value)
}

fn validate_url(value: &str) -> Result<(), RuntimeError> {
    if let Some(rest) = value.strip_prefix("https://") {
        if rest.is_empty() || rest.contains('@') {
            return Err(RuntimeError::archive_required(
                "The Windows runtime download address is invalid.",
            ));
        }
        return Ok(());
    }
    if value.starts_with("file://") && std::env::var_os("NOD_RUNTIME_ALLOW_FILE").is_some() {
        return Ok(());
    }
    Err(RuntimeError::archive_required(
        "The Windows runtime download address must use HTTPS.",
    ))
}

pub fn verify_runtime_files(
    manifest: &RuntimeManifest,
    redactor: &Redactor,
    mut progress: impl FnMut(f64, String),
) -> Result<(), RuntimeError> {
    let guest = manifest.guest_path();
    let guest_size = storage::file_size(&guest);
    if guest_size != Some(manifest.guest.bytes) {
        return Err(RuntimeError::archive_required(format!(
            "The Windows runtime image is missing or has the wrong size: {}.",
            guest.display()
        )));
    }
    progress(0.1, "Checking the private computer image…".into());
    let guest_hash = storage::sha256_file(&guest, |read, total| {
        if total > 0 {
            progress(0.1 + 0.5 * (read as f64 / total as f64), "Checking the private computer image…".into());
        }
    })
    .map_err(|error| {
        RuntimeError::archive_required(format!("The Windows runtime image could not be read: {error}"))
    })?;
    if guest_hash != manifest.guest.sha256 {
        return Err(RuntimeError::archive_required(
            "The Windows runtime image failed its integrity check.",
        ));
    }

    if let Some(expected_transport_sha) = &manifest.transport.sha256 {
        let transport = manifest.transport_path();
        if !transport.is_file() {
            return Err(RuntimeError::archive_required(
                "The Windows runtime transport is missing.",
            ));
        }
        let transport_hash = storage::sha256_file(&transport, |_, _| {}).map_err(|error| {
            RuntimeError::archive_required(format!("The transport could not be read: {error}"))
        })?;
        if transport_hash != *expected_transport_sha {
            return Err(RuntimeError::archive_required(
                "The Windows runtime transport failed its integrity check.",
            ));
        }
    }

    for image in &manifest.images {
        let path = manifest.image_path(image);
        if !path.is_file() {
            continue;
        }
        if storage::file_size(&path) != Some(image.bytes) {
            return Err(RuntimeError::archive_required(format!(
                "The {name} container has the wrong size.",
                name = image.name
            ))
            .details(path.display().to_string(), redactor));
        }
        if !image.sha256.is_empty() {
            let hash = storage::sha256_file(&path, |_, _| {}).map_err(|error| {
                RuntimeError::archive_required(format!(
                    "The {name} container could not be read: {error}",
                    name = image.name
                ))
            })?;
            if hash != image.sha256 {
                return Err(RuntimeError::archive_required(format!(
                    "The {name} container failed its integrity check.",
                    name = image.name
                ))
                .details(path.display().to_string(), redactor));
            }
        }
    }
    progress(0.65, "Runtime image verified.".into());
    Ok(())
}

#[allow(dead_code)]
pub fn check_missing_runtime(
    manifest: &RuntimeManifest,
    redactor: &Redactor,
    required: u64,
    include_images: bool,
) -> Result<(), RuntimeError> {
    let guest = manifest.guest_path();
    if !guest.is_file() {
        return Err(RuntimeError::archive_required(format!(
            "Put {} in the Nod runtime folder, then try again.",
            manifest.guest.file
        ))
        .details(guest.display().to_string(), redactor));
    }
    if !manifest.uses_wsl_transport() {
        let transport = manifest.transport_path();
        if !transport.is_file() {
            return Err(RuntimeError::archive_required(format!(
                "The Nod runtime transport {} is missing.",
                manifest.transport.file
            ))
            .details(transport.display().to_string(), redactor));
        }
    }
    if include_images {
        for image in &manifest.images {
            let path = manifest.image_path(image);
            if !path.is_file() {
                return Err(RuntimeError::archive_required(format!(
                    "The {} container is missing from the Nod runtime.",
                    image.name
                ))
                .details(path.display().to_string(), redactor));
            }
        }
    }
    let free = storage::available_bytes(&manifest.root)
        .ok_or_else(|| RuntimeError::archive_required("Nod could not check the free disk space."))?;
    if free < required.saturating_add(512 * 1024 * 1024) {
        return Err(RuntimeError::storage_full(free, required));
    }
    Ok(())
}

pub fn required_bytes(manifest: &RuntimeManifest) -> u64 {
    let present = manifest.guest_path().is_file();
    manifest
        .images
        .iter()
        .fold(0u64, |total, image| total.saturating_add(image.bytes))
        .saturating_add(manifest.guest.expanded_bytes)
        .saturating_add(if present { 0 } else { manifest.guest.bytes })
        .saturating_add(STAGING_MARGIN)
}

#[allow(dead_code)]
pub fn verify_and_reserve(
    manifest: &RuntimeManifest,
    redactor: &Redactor,
    mut progress: impl FnMut(f64, String),
) -> Result<(), RuntimeError> {
    verify_runtime_files(manifest, redactor, &mut progress)?;
    let _ = storage::check_space(&manifest.root, required_bytes(manifest))?;
    Ok(())
}

pub fn decompress_guest(
    manifest: &RuntimeManifest,
    _redactor: &Redactor,
    mut progress: impl FnMut(f64),
    mut cancelled: impl FnMut() -> bool,
) -> Result<String, RuntimeError> {
    let archive = manifest.guest_path();
    let image = manifest.root.join("guest.tar");
    if let Some(size) = storage::file_size(&image)
        && size == manifest.guest.expanded_bytes {
            return Ok(image.to_string_lossy().to_string());
        }
    let temporary = manifest.root.join(format!("guest.tar.part-{}", storage::random_suffix(8)));
    let source = File::open(&archive).map_err(|error| {
        RuntimeError::archive_required(format!("The Windows runtime image could not be opened: {error}"))
    })?;
    let mut decoder = GzDecoder::new(BufReader::new(source));
    let target = File::create(&temporary).map_err(|error| {
        RuntimeError::archive_required(format!("The Windows runtime image could not be expanded: {error}"))
    })?;
    let mut writer = BufWriter::new(target);
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut expanded = 0u64;
    let mut last_reported = std::time::Instant::now();
    loop {
        if cancelled() {
            drop(writer);
            let _ = fs::remove_file(&temporary);
            return Err(RuntimeError::new(
                crate::runtime::errors::NOD_RUNTIME_STOP_FAILED,
                "Nod setup was cancelled.",
            ));
        }
        let count = decoder.read(&mut buffer).map_err(|error| {
            RuntimeError::archive_required(format!("The Windows runtime image is damaged: {error}"))
        })?;
        if count == 0 {
            break;
        }
        expanded = expanded.saturating_add(count as u64);
        if expanded > manifest.guest.expanded_bytes {
            drop(writer);
            let _ = fs::remove_file(&temporary);
            return Err(RuntimeError::archive_required(
                "The Windows runtime image is larger than expected.",
            ));
        }
        writer.write_all(&buffer[..count]).map_err(|error| {
            RuntimeError::archive_required(format!("The Windows runtime image could not be written: {error}"))
        })?;
        if last_reported.elapsed() >= Duration::from_millis(150) {
            last_reported = std::time::Instant::now();
            progress(0.55 + 0.35 * (expanded as f64 / manifest.guest.expanded_bytes as f64));
        }
    }
    writer.flush().map_err(|error| {
        RuntimeError::archive_required(format!("The Windows runtime image could not be flushed: {error}"))
    })?;
    drop(writer);
    if expanded != manifest.guest.expanded_bytes {
        let _ = fs::remove_file(&temporary);
        return Err(RuntimeError::archive_required(
            "The Windows runtime image has an unexpected expanded size.",
        ));
    }
    storage::replace_file(&temporary, &image).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        RuntimeError::archive_required(format!(
            "The expanded runtime image could not be installed: {error}"
        ))
    })?;
    Ok(image.to_string_lossy().to_string())
}

pub fn download_guest(
    manifest: &RuntimeManifest,
    _redactor: &Redactor,
    mut progress: impl FnMut(f64, Option<u64>),
    cancelled: impl Fn() -> bool,
) -> Result<(), RuntimeError> {
    let url = manifest.guest_download_url().ok_or_else(|| {
        RuntimeError::archive_required(
            "The Nod runtime download is not configured. Set NOD_RUNTIME_URL or put guest.tar.gz in the runtime folder.",
        )
    })?;
    validate_url(&url)?;
    let destination = manifest.guest_path();
    let partial = manifest.root.join("guest.tar.gz.part");
    let existing_len = storage::file_size(&partial).unwrap_or(0);

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(120))
        .user_agent("Nod/0.1 native")
        .build();

    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut reported = 0u64;
    let started = std::time::Instant::now();

    let read_result = if let Some(path) = url.strip_prefix("file://") {
        let _ = fs::remove_file(&partial);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&partial)
            .map_err(|error| {
                RuntimeError::archive_required(format!("The runtime download could not start: {error}"))
            })?;
        let mut source = File::open(path).map_err(|error| {
            RuntimeError::archive_required(format!("The runtime file could not be opened: {error}"))
        })?;
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let count = source.read(&mut buffer).map_err(|error| {
                RuntimeError::archive_required(format!("The runtime file could not be read: {error}"))
            })?;
            if count == 0 {
                break;
            }
            if cancelled() {
                drop(file);
                let _ = fs::remove_file(&partial);
                return Err(RuntimeError::new(
                    RUNTIME_ARCHIVE_REQUIRED,
                    "Nod setup was cancelled.",
                ));
            }
            file.write_all(&buffer[..count]).map_err(|error| {
                RuntimeError::archive_required(format!("The runtime file could not be saved: {error}"))
            })?;
            hasher.update(&buffer[..count]);
            received += count as u64;
            if received.saturating_sub(reported) > 4 * 1024 * 1024 {
                reported = received;
                progress(received as f64 / manifest.guest.bytes as f64, None);
            }
        }
        file.flush().ok();
        Ok(())
    } else {
        let mut req = agent.get(&url);
        let can_resume = existing_len > 0 && existing_len < manifest.guest.bytes;
        if can_resume {
            req = req.set("Range", &format!("bytes={existing_len}-"));
        }
        let response = match req.call() {
            Ok(resp) => resp,
            Err(_) if can_resume => {
                let _ = fs::remove_file(&partial);
                agent.get(&url).call().map_err(|error| {
                    RuntimeError::archive_required(format!("The Nod runtime download failed: {error}"))
                })?
            }
            Err(error) => {
                return Err(RuntimeError::archive_required(format!(
                    "The Nod runtime download failed: {error}"
                )));
            }
        };
        let resuming = can_resume && response.status() == 206;
        let mut file = if resuming {
            let mut existing = File::open(&partial).map_err(|error| {
                RuntimeError::archive_required(format!("The partial download could not be reopened: {error}"))
            })?;
            let mut buf = vec![0u8; 1024 * 1024];
            loop {
                let n = existing.read(&mut buf).map_err(|error| {
                    RuntimeError::archive_required(format!("The partial download could not be verified: {error}"))
                })?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                received += n as u64;
            }
            drop(existing);
            reported = received;
            OpenOptions::new().append(true).open(&partial).map_err(|error| {
                RuntimeError::archive_required(format!("The partial download could not append: {error}"))
            })?
        } else {
            let _ = fs::remove_file(&partial);
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&partial)
                .map_err(|error| {
                    RuntimeError::archive_required(format!("The runtime download could not start: {error}"))
                })?
        };
        let initial_bytes = received;
        let mut reader = response.into_reader();
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    file.flush().ok();
                    break Ok(());
                }
                Ok(count) => {
                    if cancelled() {
                        break Err(RuntimeError::new(
                            NOD_WSL_COMMAND_FAILED,
                            "Nod setup was cancelled.",
                        ));
                    }
                    if received.saturating_add(count as u64) > manifest.guest.bytes {
                        let _ = fs::remove_file(&partial);
                        break Err(RuntimeError::archive_required(
                            "The Nod download is larger than expected.",
                        ));
                    }
                    if let Err(error) = file.write_all(&buffer[..count]) {
                        break Err(RuntimeError::archive_required(format!(
                            "The Nod download could not be saved: {error}"
                        )));
                    }
                    hasher.update(&buffer[..count]);
                    received += count as u64;
                    if received.saturating_sub(reported) > 4 * 1024 * 1024 {
                        reported = received;
                        let elapsed = started.elapsed().as_secs_f64();
                        let downloaded_now = received.saturating_sub(initial_bytes);
                        let seconds_left = if elapsed > 0.2 && downloaded_now > 0 {
                            let rate = downloaded_now as f64 / elapsed;
                            let rem = manifest.guest.bytes.saturating_sub(received) as f64;
                            Some((rem / rate).ceil().max(1.0) as u64)
                        } else {
                            None
                        };
                        progress(received as f64 / manifest.guest.bytes as f64, seconds_left);
                    }
                }
                Err(error) => {
                    break Err(RuntimeError::archive_required(format!(
                        "The Nod download stopped: {error}"
                    )))
                }
            }
        }
    };

    read_result?;
    if received != manifest.guest.bytes {
        let _ = fs::remove_file(&partial);
        return Err(RuntimeError::archive_required(
            "The Nod download did not finish at the expected size.",
        ));
    }
    if crate::runtime::storage::hex(&hasher.finalize()) != manifest.guest.sha256 {
        let _ = fs::remove_file(&partial);
        return Err(RuntimeError::archive_required(
            "The Nod download failed its integrity check.",
        ));
    }
    storage::replace_file(&partial, &destination).map_err(|error| {
        let _ = fs::remove_file(&partial);
        RuntimeError::archive_required(format!(
            "The downloaded runtime image could not be installed: {error}"
        ))
    })?;
    Ok(())
}

fn archive_path_is_safe(entry: &str) -> bool {
    let mut trimmed = entry.trim().replace('\\', "/");
    while let Some(stripped) = trimmed.strip_prefix("./") {
        trimmed = stripped.to_string();
    }
    let trimmed = trimmed.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return true;
    }
    is_safe_relative_path(trimmed)
}

pub fn install_runtime_archive(
    archive_path: &Path,
    paths: &RuntimePaths,
    redactor: &Redactor,
) -> Result<RuntimeManifest, RuntimeError> {
    let archive = archive_path.canonicalize().map_err(|error| {
        RuntimeError::archive_required(format!(
            "The selected runtime file could not be opened: {error}"
        ))
    })?;
    let extension = archive
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if !(extension.ends_with(".tar.gz") || extension.ends_with(".tgz") || extension.ends_with(".tar")) {
        return Err(RuntimeError::archive_required(
            "Choose a Nod runtime .tar.gz or .tgz file.",
        ));
    }
    let target = paths.import_assets_root();
    fs::create_dir_all(&paths.control_root).map_err(|error| {
        RuntimeError::archive_required(format!("The runtime folder could not be created: {error}"))
    })?;
    let temporary = paths
        .control_root
        .join(format!(".runtime-import-{}", storage::random_suffix(10)));
    fs::create_dir_all(&temporary).map_err(|error| {
        RuntimeError::archive_required(format!("The runtime staging folder could not be created: {error}"))
    })?;

    let listing = run_tar(&["-tzf", archive.to_string_lossy().as_ref()]).map_err(|detail| {
        RuntimeError::archive_required("Nod could not inspect the selected runtime file.")
            .step("assets")
            .details(detail, redactor)
    })?;
    for entry in listing.lines() {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if !archive_path_is_safe(entry) {
            let _ = fs::remove_dir_all(&temporary);
            return Err(RuntimeError::archive_required(
                "The selected runtime file contains an unsafe path.",
            )
            .details(entry, redactor));
        }
    }

    let guest_manifest_entry = listing
        .lines()
        .map(str::trim)
        .find(|line| *line == "opt/nod-assets/manifest.json" || *line == "./opt/nod-assets/manifest.json")
        .map(str::to_string);
    let has_root_manifest = listing
        .lines()
        .map(str::trim)
        .any(|line| line == "manifest.json" || line == "./manifest.json");

    if !has_root_manifest
        && let Some(entry_name) = guest_manifest_entry {
            run_tar(&[
                "-xzf",
                archive.to_string_lossy().as_ref(),
                "-C",
                temporary.to_string_lossy().as_ref(),
                entry_name.as_str(),
            ])
            .map_err(|detail| {
                let _ = fs::remove_dir_all(&temporary);
                RuntimeError::archive_required("Nod could not read manifest from the runtime file.")
                    .step("assets")
                    .details(detail, redactor)
            })?;
            let extracted_manifest = temporary.join(entry_name.trim_start_matches("./"));
            let raw_bytes = fs::read(&extracted_manifest).map_err(|error| {
                let _ = fs::remove_dir_all(&temporary);
                RuntimeError::archive_required(format!("Could not read extracted manifest: {error}"))
            })?;
            let mut raw: Value = serde_json::from_slice(&raw_bytes).map_err(|error| {
                let _ = fs::remove_dir_all(&temporary);
                RuntimeError::archive_required(format!("Extracted runtime manifest is invalid: {error}"))
            })?;
            let guest_bytes = storage::file_size(&archive).unwrap_or(0);
            let guest_sha = storage::sha256_file(&archive, |_, _| {}).map_err(|error| {
                let _ = fs::remove_dir_all(&temporary);
                RuntimeError::archive_required(format!("Could not hash runtime file: {error}"))
            })?;
            let expanded_bytes = measure_gzip_expanded_bytes(&archive).inspect_err(|_| {
                let _ = fs::remove_dir_all(&temporary);
            })?;
            if let Some(obj) = raw.as_object_mut() {
                obj.insert("transportMode".to_string(), Value::String("wsl".to_string()));
                obj.insert(
                    "guest".to_string(),
                    serde_json::json!({
                        "file": "guest.tar.gz",
                        "bytes": guest_bytes,
                        "expandedBytes": expanded_bytes,
                        "sha256": guest_sha
                    }),
                );
            }
            let _ = fs::remove_dir_all(temporary.join("opt"));
            let manifest_json = serde_json::to_vec_pretty(&raw).map_err(|error| {
                let _ = fs::remove_dir_all(&temporary);
                RuntimeError::archive_required(format!("Could not save runtime manifest: {error}"))
            })?;
            storage::atomic_write(&temporary.join("manifest.json"), &manifest_json).map_err(|error| {
                let _ = fs::remove_dir_all(&temporary);
                RuntimeError::archive_required(format!("Could not write runtime manifest: {error}"))
            })?;
            fs::copy(&archive, temporary.join("guest.tar.gz")).map_err(|error| {
                let _ = fs::remove_dir_all(&temporary);
                RuntimeError::archive_required(format!("Could not stage runtime archive: {error}"))
            })?;
            let manifest = load_manifest(&temporary.join("manifest.json"))?;
            verify_runtime_files(&manifest, redactor, |_, _| {})?;
            replace_assets_dir(&temporary, &target)?;
            return load_manifest(&target.join("manifest.json"));
        }

    run_tar(&[
        "-xzf",
        archive.to_string_lossy().as_ref(),
        "-C",
        temporary.to_string_lossy().as_ref(),
    ])
    .map_err(|detail| {
        RuntimeError::archive_required("Nod could not unpack the selected runtime file.")
            .step("assets")
            .details(detail, redactor)
    })?;

    let Some(root) = locate_manifest_root(&temporary) else {
        let _ = fs::remove_dir_all(&temporary);
        return Err(RuntimeError::archive_required(
            "The selected runtime file does not contain manifest.json.",
        ));
    };
    let manifest = load_manifest(&root.join("manifest.json"))?;
    verify_runtime_files(&manifest, redactor, |_, _| {})?;

    replace_assets_dir(&root, &target)?;
    if root != temporary {
        let _ = fs::remove_dir_all(&temporary);
    }
    load_manifest(&target.join("manifest.json"))
}

fn replace_assets_dir(source: &Path, target: &Path) -> Result<(), RuntimeError> {
    let backup = target.with_file_name(format!("runtime-assets.old-{}", storage::random_suffix(6)));
    if target.exists() {
        fs::rename(target, &backup).map_err(|error| {
            RuntimeError::archive_required(format!(
                "Could not replace previous runtime cache: {error}"
            ))
        })?;
    }
    if let Err(error) = fs::rename(source, target) {
        if backup.exists() {
            let _ = fs::rename(&backup, target);
        }
        return Err(RuntimeError::archive_required(format!(
            "The runtime could not be installed: {error}"
        )));
    }
    if backup.exists() {
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(())
}

fn locate_manifest_root(directory: &Path) -> Option<PathBuf> {
    if directory.join("manifest.json").is_file() {
        return Some(directory.to_path_buf());
    }
    let entries = fs::read_dir(directory).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.join("manifest.json").is_file() {
            return Some(path);
        }
    }
    None
}

fn run_tar(args: &[&str]) -> Result<String, String> {
    let tar = windows::tar_exe();
    let mut command = Command::new(&tar);
    command.args(args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    windows::hide_console(&mut command);
    let output = command
        .output()
        .map_err(|error| format!("Nod could not start Windows tar: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!("{stdout}\n{stderr}").trim().to_string())
    }
}

pub fn cleanup_expanded_image(path: &Path) {
    let _ = fs::remove_file(path);
}

fn measure_gzip_expanded_bytes(path: &Path) -> Result<u64, RuntimeError> {
    let source = File::open(path).map_err(|error| {
        RuntimeError::archive_required(format!("Could not open runtime archive: {error}"))
    })?;
    let mut decoder = GzDecoder::new(BufReader::new(source));
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let count = decoder.read(&mut buffer).map_err(|error| {
            RuntimeError::archive_required(format!("Runtime archive gzip stream is invalid: {error}"))
        })?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > MAX_EXPANDED_BYTES {
            return Err(RuntimeError::archive_required(
                "The Windows runtime image is larger than allowed.",
            ));
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_asset_selection_prefers_newest_runtime_descriptor() {
        let assets = vec![
            json!({
                "name": "runtime-win32-x64-older.json",
                "state": "uploaded",
                "created_at": "2026-11-25T14:10:41Z",
                "browser_download_url": "https://example.com/older.json",
                "size": 1310
            }),
            json!({
                "name": "runtime-win32-x64-newer.json",
                "state": "uploaded",
                "created_at": "2026-11-25T14:16:56Z",
                "browser_download_url": "https://example.com/newer.json",
                "size": 1310
            }),
            json!({
                "name": "runtime-win32-x64-draft.json",
                "state": "draft",
                "created_at": "2026-11-25T15:00:00Z",
                "browser_download_url": "https://example.com/draft.json",
                "size": 1310
            }),
        ];
        let chosen = select_release_asset(&assets, "runtime-win32-x64-", false).unwrap();
        assert_eq!(
            chosen.get("browser_download_url").and_then(Value::as_str),
            Some("https://example.com/newer.json")
        );
        let exact = select_release_asset(&assets, "runtime-win32-x64-older.json", true).unwrap();
        assert_eq!(
            exact.get("browser_download_url").and_then(Value::as_str),
            Some("https://example.com/older.json")
        );
    }
    use serde_json::json;

    fn valid_manifest() -> Value {
        json!({
            "version": 1,
            "platform": "win32-x64",
            "images": [
                {"name": "workspace", "reference": "nod-workspace:test", "id": format!("sha256:{}", "a".repeat(64)), "sha256": "b".repeat(64), "size": 100u64},
                {"name": "browser", "reference": "nod-browser:test", "id": format!("sha256:{}", "c".repeat(64)), "sha256": "d".repeat(64), "size": 200u64},
                {"name": "postgres", "reference": "postgres:17-alpine", "id": format!("sha256:{}", "e".repeat(64)), "sha256": "f".repeat(64), "size": 300u64}
            ],
            "guest": {
                "file": "guest.tar.gz",
                "bytes": 1000u64,
                "expandedBytes": 2000u64,
                "sha256": "1".repeat(64)
            },
            "transport": {
                "file": "bin/docker.exe",
                "sha256": "2".repeat(64)
            }
        })
    }

    #[test]
    fn manifest_accepts_expected_shape() {
        let manifest = parse_manifest(&valid_manifest(), Path::new("C:\\Nod")).unwrap();
        assert_eq!(manifest.guest.bytes, 1000);
        assert_eq!(manifest.images.len(), 3);
    }

    #[test]
    fn manifest_rejects_traversal_and_duplicates() {
        let mut value = valid_manifest();
        value["guest"]["file"] = json!("../guest.tar.gz");
        assert!(parse_manifest(&value, Path::new(".")).is_err());

        let mut value = valid_manifest();
        value["images"][1]["name"] = json!("workspace");
        assert!(parse_manifest(&value, Path::new(".")).is_err());

        let mut value = valid_manifest();
        value["images"][0]["id"] = json!("sha256:xyz");
        assert!(parse_manifest(&value, Path::new(".")).is_err());
    }

    #[test]
    fn manifest_rejects_large_or_missing_images() {
        let mut value = valid_manifest();
        value["guest"]["bytes"] = json!(9u64 * 1024 * 1024 * 1024);
        assert!(parse_manifest(&value, Path::new(".")).is_err());

        let mut value = valid_manifest();
        value["images"].as_array_mut().unwrap().pop();
        assert!(parse_manifest(&value, Path::new(".")).is_err());
    }

    #[test]
    fn guest_only_manifest_recovers_with_wsl_transport() {
        let guest_only = json!({
            "version": 1,
            "platform": "win32-x64",
            "images": [
                {"name": "workspace", "reference": "nod-workspace:test", "id": format!("sha256:{}", "a".repeat(64)), "sha256": "b".repeat(64), "size": 100u64},
                {"name": "browser", "reference": "nod-browser:test", "id": format!("sha256:{}", "c".repeat(64)), "sha256": "d".repeat(64), "size": 200u64},
                {"name": "postgres", "reference": "postgres:17-alpine", "id": format!("sha256:{}", "e".repeat(64)), "sha256": "f".repeat(64), "size": 300u64}
            ]
        });
        let dir = std::env::temp_dir().join(format!("nod-guest-manifest-{}", storage::random_suffix(8)));
        fs::create_dir_all(&dir).unwrap();
        let manifest = parse_guest_manifest(&guest_only, &dir, Some(&"7".repeat(64))).unwrap();
        assert!(manifest.uses_wsl_transport());
        assert_eq!(manifest.guest.sha256, "7".repeat(64));

        let mut cached = guest_only.clone();
        cached["transportMode"] = json!("wsl");
        fs::write(dir.join("manifest.json"), cached.to_string()).unwrap();
        let loaded = load_manifest(&dir.join("manifest.json")).unwrap();
        assert!(loaded.uses_wsl_transport());
        assert_eq!(loaded.images.len(), 3);

        // Regression test: previously cached manifests with zero-byte WSL guest metadata must reload cleanly.
        let mut legacy_zero = guest_only.clone();
        legacy_zero["transportMode"] = json!("wsl");
        legacy_zero["guest"] = json!({
            "file": "guest.tar.gz",
            "bytes": 0,
            "expandedBytes": 0,
            "sha256": "7".repeat(64)
        });
        fs::write(dir.join("manifest.json"), legacy_zero.to_string()).unwrap();
        let reloaded = load_manifest(&dir.join("manifest.json")).unwrap();
        assert!(reloaded.uses_wsl_transport());
        assert_eq!(reloaded.guest.sha256, "7".repeat(64));
        assert_eq!(reloaded.guest.bytes, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn release_descriptor_parses_and_replace_assets_dir_overwrites_safely() {
        let descriptor = json!({
            "version": 1,
            "platform": "win32-x64",
            "transportMode": "wsl",
            "releaseTag": "nod-v0.9.6",
            "images": [
                {"name": "workspace", "reference": "nod-desktop-workspace:experiment", "id": format!("sha256:{}", "a".repeat(64)), "file": "images/workspace.tar.zst", "sha256": "b".repeat(64), "size": 2033981131u64},
                {"name": "browser", "reference": "nod-desktop-browser:experiment", "id": format!("sha256:{}", "c".repeat(64)), "file": "images/browser.tar.zst", "sha256": "d".repeat(64), "size": 1293582046u64},
                {"name": "postgres", "reference": "postgres:17-alpine", "id": format!("sha256:{}", "e".repeat(64)), "file": "images/postgres.tar.zst", "sha256": "f".repeat(64), "size": 297528495u64}
            ],
            "guest": {
                "file": "guest.tar.gz",
                "bytes": 1332058697u64,
                "expandedBytes": 1554985472u64,
                "sha256": "e".repeat(64),
                "url": "https://github.com/n8n-io/nod/releases/download/nod-v0.9.6/nod-Runtime-win32-x64.tar.gz"
            }
        });
        let base = std::env::temp_dir().join(format!("nod-replace-assets-{}", storage::random_suffix(8)));
        let target = base.join("runtime-assets");
        let staging = base.join("staging");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(target.join("manifest.json"), "{}").unwrap();
        fs::write(staging.join("manifest.json"), descriptor.to_string()).unwrap();

        replace_assets_dir(&staging, &target).unwrap();
        let parsed = load_manifest(&target.join("manifest.json")).unwrap();
        assert_eq!(parsed.guest.bytes, 1332058697);
        assert_eq!(parsed.release_tag.as_deref(), Some("nod-v0.9.6"));
        let _ = fs::remove_dir_all(&base);
    }
}
