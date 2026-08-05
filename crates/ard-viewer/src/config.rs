#[cfg(not(test))]
use std::fs;
use std::io;
use std::path::PathBuf;

use ard_rs::ArdVideoQuality;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::state::{DeviceState, SavedDevice, ThemePreference};

#[cfg(not(test))]
const KEYRING_SERVICE: &str = "org.ard-rs.viewer";

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
    pub frame_interval_ms: String,
    pub key_profile: String,
    pub auto_adapt_keyboard: bool,
    pub capture_system_shortcuts: bool,
    pub show_performance_hud: bool,
    pub theme: String,
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
            frame_interval_ms: "0".into(),
            key_profile: "macOS 默认".into(),
            auto_adapt_keyboard: true,
            capture_system_shortcuts: false,
            show_performance_hud: true,
            theme: "system".into(),
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
    keyring::Entry::new(KEYRING_SERVICE, &credential_key(address, username))
        .ok()?
        .get_password()
        .ok()
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
    let entry = keyring::Entry::new(KEYRING_SERVICE, &credential_key(address, username))
        .map_err(|error| error.to_string())?;
    match password {
        Some(password) => entry.set_password(password),
        None => match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error),
        },
    }
    .map_err(|error| error.to_string())
}

#[cfg(not(test))]
fn credential_key(address: &str, username: &str) -> String {
    format!(
        "{}@{}",
        username.trim(),
        address.trim().to_ascii_lowercase()
    )
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
