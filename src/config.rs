//! App config: `config.toml` next to the executable. On first run,
//! silently migrates the Python app's `config.json` when present.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PrinterCfg {
    pub name: String,
    pub ip: String,
    pub serial: String,
    pub access_code: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub printers: Vec<PrinterCfg>,
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_path() -> PathBuf {
    exe_dir().join("config.toml")
}

/// Candidate locations of the legacy Python config.json.
fn legacy_paths() -> Vec<PathBuf> {
    vec![
        exe_dir().join("config.json"),
        // sibling checkout of the Python app (dev convenience)
        exe_dir().join("../../../bambu-handy-clone/config.json"),
    ]
}

pub fn load() -> Config {
    let path = config_path();
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(cfg) = toml::from_str::<Config>(&text)
    {
        return cfg;
    }
    // migrate legacy JSON once
    for legacy in legacy_paths() {
        if let Ok(text) = std::fs::read_to_string(&legacy)
            && let Ok(cfg) = serde_json::from_str::<Config>(&text)
        {
            save(&cfg);
            return cfg;
        }
    }
    Config::default()
}

pub fn save(cfg: &Config) {
    if let Ok(text) = toml::to_string_pretty(cfg) {
        let _ = std::fs::write(config_path(), text);
    }
}

pub const MODEL_PREFIXES: &[(&str, &str)] = &[
    ("039", "Bambu Lab A1"),
    ("030", "Bambu Lab A1 mini"),
    ("01S", "Bambu Lab P1S"),
    ("01P", "Bambu Lab P1P"),
    ("00M", "Bambu Lab X1 Carbon"),
    ("00W", "Bambu Lab X1"),
    ("094", "Bambu Lab H2D"),
];

pub fn model_from_serial(serial: &str) -> String {
    let prefix: String = serial.chars().take(3).collect::<String>().to_uppercase();
    for (p, name) in MODEL_PREFIXES {
        if *p == prefix {
            return (*name).to_string();
        }
    }
    format!("Unknown ({prefix})")
}
