//! S-CTRL's own settings (not the radio's): `$XDG_CONFIG_HOME/sctrl/config.json`.
//! Read at start, written whole when the user changes something. A missing or
//! unreadable file gives the defaults, which are the conservative ones.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::tx::TxPolicy;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Config {
    pub tx: TxPolicy,
    pub pc_speaker: PcSpeakerConfig,
    pub gui: GuiConfig,
    pub cw: CwConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CwConfig {
    /// The TX window's CW memories (at most 48 characters each).
    pub memories: Vec<String>,
}

impl Default for CwConfig {
    fn default() -> Self {
        Self {
            memories: vec!["VVV".into(), "PARIS".into(), String::new(), String::new()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PcSpeakerConfig {
    /// 0.0 ..= 1.5
    pub volume: f32,
    /// Output sink node.name; None follows the desktop's default output.
    pub pinned: Option<String>,
    /// Silence the radio's own headphones while the PC speaker plays.
    pub radio_phones_off: bool,
    /// Move other programs' audio off the radio's TX input.
    pub tx_guard: bool,
}

impl Default for PcSpeakerConfig {
    fn default() -> Self {
        Self {
            volume: 0.8,
            pinned: None,
            radio_phones_off: false,
            tx_guard: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GuiConfig {
    pub scale: f32,
    pub always_on_top: bool,
    /// Open the native view that matches the radio's page (waterfall, CW trainer, SD).
    pub follow_page: bool,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            scale: 2.0,
            always_on_top: true,
            follow_page: true,
        }
    }
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("sctrl/config.json")
}

/// The saved settings, or the defaults (with the reason when the file was
/// there but unreadable).
pub fn load() -> (Config, Option<String>) {
    let p = path();
    match std::fs::read_to_string(&p) {
        Ok(s) => match serde_json::from_str::<Config>(&s) {
            Ok(mut c) => {
                c.tx.sanitize();
                (c, None)
            }
            Err(e) => (
                Config::default(),
                Some(format!("{}: {e} (using defaults)", p.display())),
            ),
        },
        Err(_) => (Config::default(), None),
    }
}

pub fn save(c: &Config) -> Result<PathBuf> {
    let p = path();
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(c)?).with_context(|| tmp.display().to_string())?;
    std::fs::rename(&tmp, &p)?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_files_keep_defaults() {
        let c: Config = serde_json::from_str(r#"{"tx": {"tune_max_ms": 2000}}"#).unwrap();
        assert_eq!(c.tx.tune_max_ms, 2000);
        assert_eq!(c.tx.key_max_ms, TxPolicy::default().key_max_ms);
        assert_eq!(c.gui, GuiConfig::default());
    }
}
