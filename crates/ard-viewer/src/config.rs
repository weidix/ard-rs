#[cfg(not(test))]
use std::fs;
use std::io;
use std::path::PathBuf;
#[cfg(not(test))]
use std::{collections::BTreeMap, fs::File};

#[cfg(not(test))]
use aes_gcm::aead::{Aead, Payload};
#[cfg(not(test))]
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use ard_rs::ArdVideoQuality;
use directories::ProjectDirs;
#[cfg(not(test))]
use gethostname::gethostname;
use serde::{Deserialize, Serialize};
#[cfg(not(test))]
use sha2::{Digest, Sha256};

use crate::i18n::Language;
use crate::state::{DeviceState, SavedDevice, ThemePreference};

#[cfg(not(test))]
const CREDENTIAL_FILE: &str = "credentials.json";

#[cfg(not(test))]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredCredential {
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedDevice {
    pub name: String,
    pub address: String,
    #[serde(default)]
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub devices: Vec<CachedDevice>,
    pub last_address: String,
    pub last_username: String,
    pub remember_password: bool,
    pub remember_device: bool,
    pub quality: String,
    pub frame_rate: String,
    pub frame_interval_ms: String,
    pub key_profile: String,
    pub auto_adapt_keyboard: bool,
    pub capture_system_shortcuts: bool,
    pub reverse_scroll: bool,
    pub show_performance_hud: bool,
    pub theme: String,
    pub language: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            devices: Vec::new(),
            last_address: String::new(),
            last_username: String::new(),
            remember_password: false,
            remember_device: true,
            quality: "adaptive".into(),
            frame_rate: String::new(),
            frame_interval_ms: "0".into(),
            key_profile: "macOS 默认".into(),
            auto_adapt_keyboard: true,
            capture_system_shortcuts: false,
            reverse_scroll: false,
            show_performance_hud: true,
            theme: "system".into(),
            language: "zh-CN".into(),
        }
    }
}

impl AppConfig {
    pub fn load() -> Self {
        #[cfg(test)]
        return Self::default();
        #[cfg(not(test))]
        {
            let Some(path) = config_path() else {
                return Self::default();
            };
            fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default()
        }
    }

    pub fn save(&self) -> io::Result<()> {
        #[cfg(test)]
        return Ok(());
        #[cfg(not(test))]
        {
            let path = config_path().ok_or_else(|| io::Error::other("配置目录不可用"))?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let temporary = path.with_extension("json.tmp");
            fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
            fs::rename(temporary, path)
        }
    }
}

#[cfg(not(test))]
pub fn config_path() -> Option<PathBuf> {
    ProjectDirs::from("org", "ard-rs", "ARD Viewer")
        .map(|dirs| dirs.config_dir().join("config.json"))
}

pub fn export_path() -> Option<PathBuf> {
    ProjectDirs::from("org", "ard-rs", "ARD Viewer")
        .map(|dirs| dirs.data_local_dir().join("shortcuts.json"))
}

#[cfg(test)]
pub fn load_password(_address: &str, _username: &str) -> Option<String> {
    None
}

#[cfg(not(test))]
pub fn load_password(address: &str, username: &str) -> Option<String> {
    let path = credential_path()?;
    let file = File::open(path).ok()?;
    let store: BTreeMap<String, StoredCredential> = serde_json::from_reader(file).ok()?;
    let id = credential_key(address, username);
    let stored = store.get(&id)?;
    if stored.nonce.len() != 12 {
        return None;
    }
    let nonce = Nonce::from_slice(&stored.nonce);
    let cipher = Aes256Gcm::new_from_slice(&vault_key()).ok()?;
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &stored.ciphertext,
                aad: id.as_bytes(),
            },
        )
        .ok()?;
    String::from_utf8(plaintext).ok()
}

#[cfg(test)]
pub fn save_password(
    _address: &str,
    _username: &str,
    _password: Option<&str>,
) -> Result<(), String> {
    Ok(())
}

#[cfg(not(test))]
pub fn save_password(address: &str, username: &str, password: Option<&str>) -> Result<(), String> {
    let path = credential_path().ok_or_else(|| "凭据目录不可用".to_owned())?;
    let mut store: BTreeMap<String, StoredCredential> = File::open(&path)
        .ok()
        .and_then(|file| serde_json::from_reader(file).ok())
        .unwrap_or_default();
    let id = credential_key(address, username);
    match password {
        Some(password) => {
            let mut nonce = [0_u8; 12];
            getrandom::fill(&mut nonce).map_err(|error| error.to_string())?;
            let cipher =
                Aes256Gcm::new_from_slice(&vault_key()).map_err(|error| error.to_string())?;
            let ciphertext = cipher
                .encrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: password.as_bytes(),
                        aad: id.as_bytes(),
                    },
                )
                .map_err(|error| error.to_string())?;
            store.insert(
                id,
                StoredCredential {
                    nonce: nonce.to_vec(),
                    ciphertext,
                },
            );
        }
        None => {
            store.remove(&id);
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(&store).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    set_private_permissions(&temporary).map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}

#[cfg(not(test))]
fn credential_key(address: &str, username: &str) -> String {
    format!(
        "{}@{}",
        username.trim(),
        address.trim().to_ascii_lowercase()
    )
}

#[cfg(not(test))]
fn credential_path() -> Option<PathBuf> {
    config_path().and_then(|path| path.parent().map(|parent| parent.join(CREDENTIAL_FILE)))
}

#[cfg(not(test))]
fn vault_key() -> [u8; 32] {
    // Keep the vault portable across app launches without consulting a platform
    // credential service. The file itself is additionally restricted to 0600 on Unix.
    let mut hasher = Sha256::new();
    hasher.update(b"org.ard-rs.viewer credential vault v1\0");
    hasher.update(gethostname().to_string_lossy().as_bytes());
    hasher.update(std::env::var("USER").unwrap_or_default().as_bytes());
    hasher.finalize().into()
}

#[cfg(all(not(test), unix))]
fn set_private_permissions(path: &PathBuf) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(all(not(test), not(unix)))]
fn set_private_permissions(_path: &PathBuf) -> io::Result<()> {
    Ok(())
}

pub fn devices_from_cache(config: &AppConfig) -> Vec<SavedDevice> {
    config
        .devices
        .iter()
        .map(|device| SavedDevice {
            name: device.name.clone(),
            address: device.address.clone(),
            username: device.username.clone(),
            state: DeviceState::Saved,
        })
        .collect()
}

pub fn quality_from_cache(value: &str) -> ArdVideoQuality {
    match value {
        "low" => ArdVideoQuality::Low,
        "medium" => ArdVideoQuality::Medium,
        "high" => ArdVideoQuality::High,
        "full" => ArdVideoQuality::Full,
        _ => ArdVideoQuality::Adaptive,
    }
}

pub fn quality_to_cache(value: ArdVideoQuality) -> &'static str {
    match value {
        ArdVideoQuality::Low => "low",
        ArdVideoQuality::Medium => "medium",
        ArdVideoQuality::High => "high",
        ArdVideoQuality::Adaptive => "adaptive",
        ArdVideoQuality::Full => "full",
    }
}

pub fn theme_from_cache(value: &str) -> ThemePreference {
    match value {
        "light" => ThemePreference::Light,
        "dark" => ThemePreference::Dark,
        _ => ThemePreference::System,
    }
}

pub fn theme_to_cache(value: ThemePreference) -> &'static str {
    match value {
        ThemePreference::System => "system",
        ThemePreference::Light => "light",
        ThemePreference::Dark => "dark",
    }
}

pub fn language_from_cache(value: &str) -> Language {
    Language::from_code(value)
}
