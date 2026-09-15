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
    vec![exe_dir().join("config.json")]
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
    ("01P", "Bambu Lab P1S"),
    ("01S", "Bambu Lab P1P"),
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

#[cfg(test)]
mod tests {
    use super::model_from_serial;

    #[test]
    fn p1_prefixes_match_bambu_serials() {
        // Bambu wiki: P1S serials start with 01P, P1P serials with 01S
        assert_eq!(model_from_serial("01P00A000000000"), "Bambu Lab P1S");
        assert_eq!(model_from_serial("01S00A000000000"), "Bambu Lab P1P");
    }

    #[test]
    fn prefix_lookup_is_case_insensitive() {
        assert_eq!(model_from_serial("01p00a000000000"), "Bambu Lab P1S");
        assert_eq!(model_from_serial("039xx"), "Bambu Lab A1");
    }

    #[test]
    fn unknown_prefix_is_reported() {
        assert_eq!(model_from_serial("ZZZ123"), "Unknown (ZZZ)");
        assert_eq!(model_from_serial(""), "Unknown ()");
    }
}
