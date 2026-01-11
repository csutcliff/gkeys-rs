//! Configuration loading and parsing

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub keyboard_mapping: String,
    #[serde(default = "default_notify")]
    pub notify: StringBool,
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

#[derive(Debug, Deserialize)]
pub struct Profile {
    #[serde(flatten)]
    pub macros: HashMap<String, Macro>,
}

#[derive(Debug, Deserialize)]
pub struct Macro {
    pub hotkey_type: HotkeyType,
    #[serde(rename = "do", default)]
    pub action: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
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
