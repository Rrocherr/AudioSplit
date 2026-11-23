use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const MAX_VOLUME: u32 = 100;
pub const MAX_DELAY_MS: u32 = 1000;

fn default_true() -> bool {
    true
}

fn default_volume() -> u32 {
    MAX_VOLUME
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_volume")]
    pub volume: u32,
    #[serde(default)]
    pub delay_ms: u32,
    #[serde(default)]
    pub last_name: String,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            volume: MAX_VOLUME,
            delay_ms: 0,
            last_name: String::new(),
        }
    }
}

impl DeviceConfig {
    pub fn sanitize(&mut self) {
        self.volume = self.volume.min(MAX_VOLUME);
        self.delay_ms = self.delay_ms.min(MAX_DELAY_MS);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    #[serde(default = "default_true")]
    pub routing_enabled: bool,
    #[serde(default)]
    pub start_minimized: bool,
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            routing_enabled: true,
            start_minimized: false,
            devices: BTreeMap::new(),
        }
    }
}

impl AppConfig {
    pub fn sanitize(&mut self) {
        for dev in self.devices.values_mut() {
            dev.sanitize();
        }
    }

    pub fn config_path() -> PathBuf {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                return dir.join("audiosplit.json");
            }
        }
        PathBuf::from("audiosplit.json")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::config_path())
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(content) = fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<Self>(&content) {
            Ok(mut cfg) => {
                cfg.sanitize();
                cfg
            }
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::config_path())
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let mut sanitized = self.clone();
        sanitized.sanitize();
        let json = serde_json::to_string_pretty(&sanitized)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }

        let tmp_path = path.with_extension("json.tmp");
        if fs::write(&tmp_path, &json).is_ok() {
            if fs::rename(&tmp_path, path).is_ok() {
                return Ok(());
            }
            let _ = fs::remove_file(&tmp_path);
        }

        fs::write(path, &json)
    }

    pub fn get_device(&self, id: &str) -> DeviceConfig {
        self.devices.get(id).cloned().unwrap_or_default()
    }

    pub fn ensure_device_mut(&mut self, id: &str, name: &str) -> &mut DeviceConfig {
        let entry = self.devices.entry(id.to_string()).or_default();
        if !name.is_empty() {
            entry.last_name = name.to_string();
        }
        entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "audiosplit_test_{}_{}_{}.json",
            std::process::id(),
            nanos,
            name
        ))
    }

    #[test]
    fn default_config_values() {
        let cfg = AppConfig::default();
        assert!(cfg.routing_enabled);
        assert!(!cfg.start_minimized);
        assert!(cfg.devices.is_empty());

        let dev = cfg.get_device("missing-id");
        assert!(!dev.enabled);
        assert_eq!(dev.volume, 100);
        assert_eq!(dev.delay_ms, 0);
    }

    #[test]
    fn roundtrip_and_clamping() {
        let path = temp_test_path("roundtrip");
        let mut cfg = AppConfig {
            routing_enabled: false,
            start_minimized: true,
            devices: BTreeMap::new(),
        };
        cfg.devices.insert(
            "dev-1".to_string(),
            DeviceConfig {
                enabled: true,
                volume: 250,
                delay_ms: 9999,
                last_name: "Headphones".to_string(),
            },
        );

        cfg.save_to(&path).expect("save should succeed");
        let loaded = AppConfig::load_from(&path);
        let _ = fs::remove_file(&path);

        assert!(!loaded.routing_enabled);
        assert!(loaded.start_minimized);
        let dev = loaded.devices.get("dev-1").expect("device should exist");
        assert!(dev.enabled);
        assert_eq!(dev.volume, MAX_VOLUME);
        assert_eq!(dev.delay_ms, MAX_DELAY_MS);
        assert_eq!(dev.last_name, "Headphones");
    }

    #[test]
    fn corrupt_or_missing_file_returns_default() {
        let missing = temp_test_path("missing");
        assert_eq!(AppConfig::load_from(&missing), AppConfig::default());

        let corrupt = temp_test_path("corrupt");
        fs::write(&corrupt, "{ not valid json !!!").expect("write corrupt");
        let loaded = AppConfig::load_from(&corrupt);
        let _ = fs::remove_file(&corrupt);

        assert_eq!(loaded, AppConfig::default());
    }
}
