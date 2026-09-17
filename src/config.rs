//! App config: `config.toml` next to the executable. On first run,
//! migrates the Python app's `config.json` when present.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PrinterCfg {
    pub name: String,
    pub ip: String,
    pub serial: String,
    pub access_code: String,
}

/// The `[files]` table (design doc 5.6): the disk cache's size cap, in
/// gigabytes. It is written before `printers` so the file stays valid TOML —
/// a plain table after an array of tables would belong to the last printer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FilesCfg {
    #[serde(default = "default_cache_cap_gb")]
    pub cache_cap_gb: u64,
}

/// Design doc 5.6: the cache cap defaults to 5 GB.
fn default_cache_cap_gb() -> u64 {
    5
}

impl Default for FilesCfg {
    fn default() -> Self {
        Self { cache_cap_gb: default_cache_cap_gb() }
    }
}

impl FilesCfg {
    /// The cap in bytes. A cap of 0 would evict every file the moment it
    /// lands, so it is read as the default instead.
    pub fn cache_cap_bytes(&self) -> u64 {
        let gb = match self.cache_cap_gb {
            0 => default_cache_cap_gb(),
            gb => gb,
        };
        gb.saturating_mul(1024 * 1024 * 1024)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub files: FilesCfg,
    #[serde(default)]
    pub printers: Vec<PrinterCfg>,
}

/// A config as read at start. `migrated`: it came from the legacy JSON and
/// has not been written as `config.toml` yet.
pub struct Loaded {
    pub cfg: Config,
    pub migrated: bool,
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

/// The serial as the certificate check compares it with the CN (design doc
/// 5.3): trimmed and ASCII-uppercased, on save and on load.
pub fn normalize_serial(serial: &str) -> String {
    serial.trim().to_ascii_uppercase()
}

fn normalized(mut cfg: Config) -> Config {
    for printer in &mut cfg.printers {
        printer.serial = normalize_serial(&printer.serial);
    }
    cfg
}

/// `config.toml` and whether it may be written. The file holds every
/// printer's access code, so after a load that failed for any reason but a
/// missing file, every save is refused until the app starts with a readable
/// file: the file is never replaced with defaults (design doc 5.9).
pub struct Store {
    path: PathBuf,
    blocked: bool,
}

impl Store {
    /// Loads `config.toml` next to the executable (see `load_from`).
    pub fn load() -> (Self, io::Result<Loaded>) {
        Self::open(config_path(), &legacy_paths())
    }

    fn open(path: PathBuf, legacy: &[PathBuf]) -> (Self, io::Result<Loaded>) {
        let loaded = load_from(&path, legacy);
        (Self { blocked: loaded.is_err(), path }, loaded)
    }

    /// The load failed: `save` writes nothing.
    pub fn is_blocked(&self) -> bool {
        self.blocked
    }

    /// Saves atomically (`save_to`). Refused, without touching the file,
    /// while the store is blocked.
    pub fn save(&self, cfg: &Config) -> io::Result<()> {
        if self.blocked {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied,
                "config.toml can't be read; nothing is written until it is \
                 fixed"));
        }
        save_to(&self.path, cfg)
    }
}

/// Only a missing `config.toml` is a first start (legacy JSON or defaults).
/// Any other read or parse error is returned untouched: the caller shows it,
/// and `Store` writes nothing.
fn load_from(path: &Path, legacy: &[PathBuf]) -> io::Result<Loaded> {
    match std::fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(cfg) => Ok(Loaded { cfg: normalized(cfg), migrated: false }),
            // the parser's message quotes the file, which holds access
            // codes: report the line only
            Err(e) => {
                let line = e.span()
                    .map(|s| text[..s.start.min(text.len())].lines().count())
                    .map(|n| format!(" (line {})", n.max(1)))
                    .unwrap_or_default();
                Err(io::Error::new(io::ErrorKind::InvalidData,
                                   format!("not valid TOML{line}")))
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            for legacy in legacy {
                if let Ok(text) = std::fs::read_to_string(legacy)
                    && let Ok(cfg) = serde_json::from_str::<Config>(&text)
                {
                    return Ok(Loaded { cfg: normalized(cfg), migrated: true });
                }
            }
            Ok(Loaded { cfg: Config::default(), migrated: false })
        }
        Err(e) => Err(e),
    }
}

/// Writes `config.toml` atomically: a uniquely named temp file in the same
/// directory, synced, then renamed over the target (which replaces it on
/// Windows). The file holds every printer's access code, so a torn write or
/// a silently failed save is not acceptable: callers show the error.
fn save_to(path: &Path, cfg: &Config) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let text = toml::to_string_pretty(&normalized(cfg.clone()))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let name = path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".into());
    let tmp = path.with_file_name(format!(
        "{name}.{}.{}.tmp", std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)));
    let result = write_synced(&tmp, text.as_bytes())
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        // best effort; the error that matters is the one returned
        std::fs::remove_file(&tmp).ok();
    }
    result
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Model names by the first 3 serial characters, as Bambu Studio does
/// (`dev_id.substr(0, 3)`).
pub const MODEL_PREFIXES: &[(&str, &str)] = &[
    ("039", "Bambu Lab A1"),
    ("030", "Bambu Lab A1 mini"),
    ("01P", "Bambu Lab P1S"),
    ("01S", "Bambu Lab P1P"),
    ("00M", "Bambu Lab X1 Carbon"),
    ("00W", "Bambu Lab X1"),
    ("03W", "Bambu Lab X1E"),
    ("094", "Bambu Lab H2D"),
    ("093", "Bambu Lab H2S"),
    ("239", "Bambu Lab H2D Pro"),
    ("31B", "Bambu Lab H2C"),
    ("22E", "Bambu Lab P2S"),
    ("20P", "Bambu Lab X2D"),
    ("26A", "Bambu Lab A2L"),
];

fn serial_prefix(serial: &str) -> String {
    serial.chars().take(3).collect::<String>().to_uppercase()
}

fn model_name(serial: &str) -> Option<&'static str> {
    let prefix = serial_prefix(serial);
    MODEL_PREFIXES.iter().find(|(p, _)| *p == prefix).map(|(_, name)| *name)
}

pub fn model_from_serial(serial: &str) -> String {
    match model_name(serial) {
        Some(name) => name.to_string(),
        None => format!("Unknown ({})", serial_prefix(serial)),
    }
}

/// Certificate generation seen on a model's printers (design doc 5.3,
/// Models). It never grants trust: it picks the refusal wording, and for
/// V2 models skips the FTPS connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertGeneration {
    /// X.509 v1 leaf issued by BBL CA: A1 and P1S (owner's printers), X1
    /// Carbon (third-party capture)
    Legacy,
    /// BBL Device CA <code>-V2 under BBL CA2 RSA (third-party chains)
    V2,
    /// every other model, and prefixes not in the table
    NotObserved,
}

pub fn cert_generation(serial: &str) -> CertGeneration {
    match serial_prefix(serial).as_str() {
        "039" | "01P" | "00M" => CertGeneration::Legacy,
        "31B" | "22E" | "20P" => CertGeneration::V2,
        _ => CertGeneration::NotObserved,
    }
}

/// The model as refusal texts name it: never "Unknown (31B)".
fn model_in_text(serial: &str) -> &'static str {
    model_name(serial).unwrap_or("this printer model")
}

/// H2C, P2S and X2D present V2 certificates: file access is disabled and no
/// FTPS connection is attempted.
pub fn files_refused_by_name(serial: &str) -> Option<String> {
    (cert_generation(serial) == CertGeneration::V2).then(|| format!(
        "{} uses Bambu's newer certificate authority (BBL CA2), which this \
         version does not verify yet; file access is disabled for it.",
        model_in_text(serial)))
}

/// The verifier found another authority on a model whose certificate
/// generation was not observed.
pub fn other_authority_refusal(serial: &str) -> String {
    let model = model_in_text(serial);
    let mut chars = model.chars();
    let model: String = chars.next().map(|c| c.to_ascii_uppercase())
        .into_iter().chain(chars).collect();
    format!("{model}: not tested on this model. Its FTP certificate is not \
             from the authority this version verifies (BBL CA), so the \
             connection was refused and the access code was not sent over it.")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{CertGeneration, Config, FilesCfg, MODEL_PREFIXES, PrinterCfg,
                Store, cert_generation, files_refused_by_name, load_from,
                model_from_serial, normalize_serial, other_authority_refusal,
                save_to};

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

    /// T25: Studio's printer profiles, Bambu's HMS data and the wiki.
    #[test]
    fn model_prefixes_match_studio_table() {
        let table = [
            ("039", "Bambu Lab A1"), ("030", "Bambu Lab A1 mini"),
            ("01P", "Bambu Lab P1S"), ("01S", "Bambu Lab P1P"),
            ("00M", "Bambu Lab X1 Carbon"), ("00W", "Bambu Lab X1"),
            ("03W", "Bambu Lab X1E"), ("094", "Bambu Lab H2D"),
            ("093", "Bambu Lab H2S"), ("239", "Bambu Lab H2D Pro"),
            ("31B", "Bambu Lab H2C"), ("22E", "Bambu Lab P2S"),
            ("20P", "Bambu Lab X2D"), ("26A", "Bambu Lab A2L"),
        ];
        assert_eq!(MODEL_PREFIXES.len(), 14);
        for (prefix, name) in table {
            assert_eq!(model_from_serial(&format!("{prefix}00A0000000000")),
                       name);
        }
        // three characters only: real X2D serials differ from the fourth on
        assert_eq!(model_from_serial("20PX"), "Bambu Lab X2D");
        // no "X2C" printer exists
        assert!(MODEL_PREFIXES.iter().all(|(_, n)| !n.contains("X2C")));
    }

    #[test]
    fn v2_models_are_refused_by_name() {
        for (prefix, model) in [("31B", "Bambu Lab H2C"),
                                ("22E", "Bambu Lab P2S"),
                                ("20P", "Bambu Lab X2D")] {
            let serial = format!("{prefix}0000000000000");
            assert_eq!(cert_generation(&serial), CertGeneration::V2);
            assert_eq!(files_refused_by_name(&serial), Some(format!(
                "{model} uses Bambu's newer certificate authority (BBL CA2), \
                 which this version does not verify yet; file access is \
                 disabled for it.")));
        }
        for prefix in ["039", "030", "01P", "01S", "00M", "00W", "03W",
                       "094", "093", "239", "26A", "ZZZ"] {
            assert_eq!(files_refused_by_name(&format!("{prefix}0000")), None,
                       "{prefix}");
        }
    }

    #[test]
    fn unobserved_models_are_named_in_the_authority_refusal() {
        assert_eq!(other_authority_refusal("0940000000000"),
                   "Bambu Lab H2D: not tested on this model. Its FTP \
                    certificate is not from the authority this version \
                    verifies (BBL CA), so the connection was refused and the \
                    access code was not sent over it.");
        for serial in ["039", "01P", "00M"] {
            assert_eq!(cert_generation(serial), CertGeneration::Legacy);
        }
        for serial in ["030", "01S", "00W", "03W", "094", "093", "239",
                       "26A", "ZZZ", ""] {
            assert_eq!(cert_generation(serial), CertGeneration::NotObserved,
                       "{serial}");
        }
    }

    /// T25: an unrecognised prefix is "this printer model".
    #[test]
    fn no_refusal_text_shows_unknown_with_a_prefix() {
        for serial in ["ZZZ0000000000", "31C0000000000", "", "0"] {
            let texts = [other_authority_refusal(serial),
                         files_refused_by_name(serial).unwrap_or_default()];
            for text in texts {
                assert!(!text.contains("Unknown"), "{text}");
                assert!(!text.contains("ZZZ") && !text.contains("31C"),
                        "{text}");
            }
        }
        assert!(other_authority_refusal("ZZZ0")
            .starts_with("This printer model: not tested on this model."));
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "bambu-control-test-{label}-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn entries(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0).unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn printer(serial: &str) -> PrinterCfg {
        PrinterCfg {
            name: "P1S".into(),
            ip: "192.0.2.15".into(),
            serial: serial.into(),
            access_code: "12345678".into(),
        }
    }

    /// T26
    #[test]
    fn config_save_is_atomic_and_reports_errors() {
        let dir = TempDir::new("save");
        let path = dir.path().join("config.toml");
        let cfg = Config { printers: vec![printer("01P00A000000001")],
                           ..Default::default() };
        save_to(&path, &cfg).unwrap();
        assert_eq!(dir.entries(), ["config.toml"], "no temp file left");
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.contains("01P00A000000001"));

        // a save into a missing directory fails and leaves nothing behind
        let missing = dir.path().join("missing").join("config.toml");
        assert!(save_to(&missing, &cfg).is_err());
        assert_eq!(dir.entries(), ["config.toml"]);

        // a save that cannot replace the file returns Err and leaves the
        // previous file intact, without a stray temp file
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let lock = std::fs::OpenOptions::new().read(true).share_mode(0)
                .open(&path).unwrap();
            let changed = Config {
                printers: vec![printer("01P00A000000002")],
                ..Default::default() };
            assert!(save_to(&path, &changed).is_err());
            drop(lock);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
            assert_eq!(dir.entries(), ["config.toml"]);
        }

        // a save replaces the file and never writes into it: a reader that
        // opened the old file still reads the whole old content
        #[cfg(windows)]
        {
            use std::io::Read;
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
            let mut old = std::fs::OpenOptions::new().read(true)
                .share_mode(0x7).open(&path).unwrap();
            let changed = Config {
                printers: vec![printer("01P00A000000004")],
                ..Default::default() };
            save_to(&path, &changed).unwrap();
            let mut seen = String::new();
            old.read_to_string(&mut seen).unwrap();
            drop(old);
            assert_eq!(seen, first);
            assert!(std::fs::read_to_string(&path).unwrap()
                .contains("01P00A000000004"));
            assert_eq!(dir.entries(), ["config.toml"]);
        }

        // and a later save replaces the whole content
        let two = Config {
            printers: vec![printer("01P00A000000003"), printer("0390000")],
            ..Default::default()
        };
        save_to(&path, &two).unwrap();
        let loaded = load_from(&path, &[]).unwrap();
        assert_eq!(loaded.cfg.printers, two.printers);
        assert_eq!(dir.entries(), ["config.toml"]);
    }

    /// T26
    #[test]
    fn corrupt_config_is_not_overwritten() {
        let dir = TempDir::new("corrupt");
        let path = dir.path().join("config.toml");
        let legacy = dir.path().join("config.json");
        std::fs::write(&legacy, r#"{"printers":[{"name":"a","ip":"b",
            "serial":"c","access_code":"SECRETCODE"}]}"#).unwrap();
        let broken = "[[printers]]\nname = \"P1S\"\naccess_code = SECRETCODE\n";
        std::fs::write(&path, broken).unwrap();
        let err = load_from(&path, std::slice::from_ref(&legacy))
            .err().expect("a broken file is not a missing one");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        // the message never quotes the file
        assert!(!err.to_string().contains("SECRETCODE"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);

        // unreadable bytes are an error too, not a first start
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert!(load_from(&path, &[]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), [0xff, 0xfe, 0x00]);

        // only a missing file falls back to the legacy JSON
        std::fs::remove_file(&path).unwrap();
        let loaded = load_from(&path, &[legacy]).unwrap();
        assert!(loaded.migrated);
        assert_eq!(loaded.cfg.printers.len(), 1);
        assert!(!path.exists(), "load never writes");
        let loaded = load_from(&path, &[]).unwrap();
        assert!(!loaded.migrated && loaded.cfg.printers.is_empty());
    }

    /// T26: after a failed load the store writes nothing, whoever calls it.
    #[test]
    fn save_after_a_failed_load_is_refused() {
        let dir = TempDir::new("blocked");
        let path = dir.path().join("config.toml");
        let cfg = Config { printers: vec![printer("01P00A000000001")],
                           ..Default::default() };
        let broken = "[[printers]]\nname = \"P1S\"\naccess_code = SECRETCODE\n";
        std::fs::write(&path, broken).unwrap();
        let (store, loaded) = Store::open(path.clone(), &[]);
        assert!(loaded.is_err() && store.is_blocked());
        let err = store.save(&cfg).expect_err("a blocked store writes nothing");
        assert!(!err.to_string().contains("SECRETCODE"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        assert_eq!(dir.entries(), ["config.toml"], "no temp file either");

        // unreadable bytes block it too
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        let (store, _) = Store::open(path.clone(), &[]);
        assert!(store.save(&cfg).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), [0xff, 0xfe, 0x00]);

        // a missing file is a first start: saves are written
        std::fs::remove_file(&path).unwrap();
        let (store, loaded) = Store::open(path.clone(), &[]);
        assert!(loaded.is_ok() && !store.is_blocked());
        store.save(&cfg).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap()
            .contains("01P00A000000001"));
    }

    /// T26
    #[test]
    fn serial_is_trimmed_and_uppercased_on_save_and_load() {
        assert_eq!(normalize_serial(" 01p00a000000001\t"), "01P00A000000001");
        let dir = TempDir::new("serial");
        let path = dir.path().join("config.toml");
        save_to(&path, &Config {
            printers: vec![printer(" 01p00a0001 ")],
            ..Default::default() }).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("serial = \"01P00A0001\""), "{text}");
        std::fs::write(&path, "[[printers]]\nname = \"a\"\nip = \"b\"\n\
            serial = \" 039x \"\naccess_code = \"c\"\n").unwrap();
        let loaded = load_from(&path, &[]).unwrap();
        assert_eq!(loaded.cfg.printers[0].serial, "039X");
    }

    /// Design doc 5.6: the cache cap is a `[files]` table, it defaults to
    /// 5 GB, and a config written before the table still loads.
    #[test]
    fn the_files_table_carries_the_cache_cap() {
        assert_eq!(FilesCfg::default().cache_cap_gb, 5);
        assert_eq!(FilesCfg::default().cache_cap_bytes(),
                   5 * 1024 * 1024 * 1024);
        // a cap of 0 would evict every file the moment it landed
        assert_eq!(FilesCfg { cache_cap_gb: 0 }.cache_cap_bytes(),
                   FilesCfg::default().cache_cap_bytes());

        let dir = TempDir::new("files");
        let path = dir.path().join("config.toml");
        let cfg = Config {
            files: FilesCfg { cache_cap_gb: 12 },
            printers: vec![printer("01P00A000000001")],
        };
        save_to(&path, &cfg).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[files]")
                && text.contains("cache_cap_gb = 12"), "{text}");
        assert_eq!(load_from(&path, &[]).unwrap().cfg.files.cache_cap_gb, 12);

        // every printer still round-trips next to the new table
        assert_eq!(load_from(&path, &[]).unwrap().cfg.printers,
                   cfg.printers);

        // a config written before the table reads as the default, and is
        // not rejected
        std::fs::write(&path, "[[printers]]\nname = \"a\"\nip = \"b\"\n\
            serial = \"039X\"\naccess_code = \"c\"\n").unwrap();
        let loaded = load_from(&path, &[]).unwrap();
        assert_eq!(loaded.cfg.files.cache_cap_gb, 5);
        assert_eq!(loaded.cfg.printers.len(), 1);
    }
}
