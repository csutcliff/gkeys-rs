//! Configuration loading and parsing

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub keyboard_mapping: String,
    #[serde(default = "default_notify")]
    pub notify: StringBool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rgb_color: Option<RgbColor>,
    pub profiles: HashMap<String, Profile>,
}

fn default_notify() -> StringBool {
    StringBool(true)
}

/// Handle Python-style "True"/"False" strings as bools
#[derive(Debug, Clone)]
pub struct StringBool(pub bool);

impl Default for StringBool {
    fn default() -> Self {
        Self(false)
    }
}

impl<'de> Deserialize<'de> for StringBool {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum BoolOrString {
            Bool(bool),
            String(String),
        }

        match BoolOrString::deserialize(deserializer)? {
            BoolOrString::Bool(b) => Ok(StringBool(b)),
            BoolOrString::String(s) => Ok(StringBool(s.eq_ignore_ascii_case("true"))),
        }
    }
}

impl Serialize for StringBool {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Serialize as boolean for clean JSON output
        serializer.serialize_bool(self.0)
    }
}

/// RGB color for keyboard LED configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Profile {
    #[serde(flatten)]
    pub macros: HashMap<String, Macro>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Macro {
    pub hotkey_type: HotkeyType,
    #[serde(rename = "do", default, skip_serializing_if = "String::is_empty")]
    pub action: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HotkeyType {
    Run,
    Shortcut,
    Typeout,
    Uinput,
    Sequence,
    Nothing,
}

impl Config {
    /// Load config from the default location
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        Self::load_from(&path)
    }

    /// Load config from a specific path
    pub fn load_from(path: &PathBuf) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config from {}", path.display()))?;
        let config: Config = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse config from {}", path.display()))?;
        Ok(config)
    }

    /// Get the default config path
    pub fn config_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .context("Could not determine config directory")?
            .join("gkeys-rs");
        Ok(config_dir.join("config.json"))
    }

    /// Get a macro definition for the given profile and key
    pub fn get_macro(&self, profile: &str, macro_name: &str) -> Option<&Macro> {
        self.profiles.get(profile)?.macros.get(macro_name)
    }

    /// Set a macro definition for the given profile and key
    pub fn set_macro(&mut self, profile: &str, macro_name: &str, macro_def: Macro) {
        let profile_entry = self
            .profiles
            .entry(profile.to_string())
            .or_insert_with(|| Profile {
                macros: HashMap::new(),
            });
        profile_entry.macros.insert(macro_name.to_string(), macro_def);
    }

    /// Save config to file, creating a backup first
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        let backup_path = path.with_extension("json.bak");

        // Create backup if file exists
        if path.exists() {
            fs::copy(&path, &backup_path)
                .with_context(|| format!("Failed to create backup at {}", backup_path.display()))?;
            log::info!("Created config backup at {}", backup_path.display());
        }

        // Write new config with pretty formatting
        let json = serde_json::to_string_pretty(self)
            .context("Failed to serialize config")?;
        let mut file = fs::File::create(&path)
            .with_context(|| format!("Failed to create config at {}", path.display()))?;
        file.write_all(json.as_bytes())
            .with_context(|| format!("Failed to write config to {}", path.display()))?;

        log::info!("Saved config to {}", path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config() {
        let json = r#"{
            "notify": "True",
            "profiles": {
                "MEMORY_1": {
                    "MACRO_1": { "hotkey_type": "run", "do": "echo hello" },
                    "MACRO_2": { "hotkey_type": "nothing" }
                }
            }
        }"#;

        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.notify.0);
        assert!(config.profiles.contains_key("MEMORY_1"));

        let m1 = config.get_macro("MEMORY_1", "MACRO_1").unwrap();
        assert_eq!(m1.hotkey_type, HotkeyType::Run);
        assert_eq!(m1.action, "echo hello");
    }
}
