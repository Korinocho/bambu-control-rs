//! HMS error lookup against Bambu Lab's public error database
//! (e.bambulab.com — same source the Handy app uses) + wiki links.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
static FETCHING: AtomicBool = AtomicBool::new(false);

const QUERY_URL: &str = "https://e.bambulab.com/query.php?lang=en";

/// 16-hex uppercase error code from the report's attr + code words.
pub fn ecode(attr: u64, code: u64) -> String {
    format!("{attr:08X}{code:08X}")
}

/// "0300-0D00-0001-000B" presentation/wiki form.
pub fn dashed(ecode: &str) -> String {
    ecode
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("-")
}

/// Human description for an error code, when the DB is loaded.
pub fn lookup(ecode: &str) -> Option<String> {
    let cache = CACHE.lock().unwrap();
    let map = cache.as_ref()?;
    if let Some(intro) = map.get(ecode) {
        return Some(intro.clone());
    }
    // The A1's AMS Lite reports 12xx codes that mirror the standard
    // AMS 07xx series, which is the one present in Bambu's DB.
    if let Some(rest) = ecode.strip_prefix("12") {
        let alt = format!("07{rest}");
        if let Some(intro) = map.get(&alt) {
            return Some(intro.replace("AMS A ", "AMS Lite "));
        }
    }
    None
}

/// Kick off the one-time background download of the error database.
pub fn ensure_loaded(ctx: &egui::Context) {
    if CACHE.lock().unwrap().is_some()
        || FETCHING.swap(true, Ordering::SeqCst)
    {
        return;
    }
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let map = fetch_db().unwrap_or_default();
        *CACHE.lock().unwrap() = Some(map);
        ctx.request_repaint();
    });
}

fn fetch_db() -> anyhow::Result<HashMap<String, String>> {
    let mut res = ureq::get(QUERY_URL)
        .header("User-Agent", "bambu-control/1.0")
        .call()?;
    let body = res.body_mut().read_to_string()?;
    let data: serde_json::Value = serde_json::from_str(&body)?;
    let mut map = HashMap::new();
    if let Some(entries) = data
        .pointer("/data/device_hms/en")
        .and_then(|v| v.as_array())
    {
        for entry in entries {
            if let (Some(code), Some(intro)) = (
                entry.get("ecode").and_then(|v| v.as_str()),
                entry.get("intro").and_then(|v| v.as_str()),
            ) {
                map.insert(code.to_uppercase(), intro.to_string());
            }
        }
    }
    // device errors (print_error) share the page too; merge if present
    if let Some(entries) = data
        .pointer("/data/device_error/en")
        .and_then(|v| v.as_array())
    {
        for entry in entries {
            if let (Some(code), Some(intro)) = (
                entry.get("ecode").and_then(|v| v.as_str()),
                entry.get("intro").and_then(|v| v.as_str()),
            ) {
                map.entry(code.to_uppercase())
                    .or_insert_with(|| intro.to_string());
            }
        }
    }
    Ok(map)
}

