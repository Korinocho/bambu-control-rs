//! Latest-firmware lookup — port of `core/firmware.py`.
//!
//! Primary: Bambu's official firmware-download page (printerMap JSON in
//! __NEXT_DATA__, all models at once). Fallback: community GitHub
//! mirror of the OTA feed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex_lite::Regex;

const OFFICIAL_URL: &str =
    "https://bambulab.com/en/support/firmware-download/a1";
const MIRROR_BASE: &str = "https://raw.githubusercontent.com/lunDreame/\
                           user-bambulab-firmware/main/assets/";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
                  AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 \
                  Safari/537.36";
const TTL: Duration = Duration::from_secs(6 * 3600);

/// model_from_serial() name -> key in the official page's printerMap
fn model_key(model: &str) -> Option<&'static str> {
    Some(match model {
        "Bambu Lab A1" => "a1",
        "Bambu Lab A1 mini" => "a1-mini",
        "Bambu Lab P1S" | "Bambu Lab P1P" => "p1",
        "Bambu Lab X1 Carbon" | "Bambu Lab X1" => "x1",
        "Bambu Lab X1E" => "x1e",
        "Bambu Lab H2D" => "h2d",
        _ => return None,
    })
}

fn mirror_feed(model: &str) -> Option<&'static str> {
    Some(match model {
        "Bambu Lab A1" => "a1_ams.json",
        "Bambu Lab A1 mini" => "a1_mini_ams.json",
        "Bambu Lab P1S" | "Bambu Lab P1P" => "p1_series_ams.json",
        "Bambu Lab X1 Carbon" | "Bambu Lab X1" => "x1_series_ams.json",
        "Bambu Lab X1E" => "x1e_ams.json",
        _ => return None,
    })
}

fn ver_tuple(version: &str) -> Option<Vec<u64>> {
    let parts: Result<Vec<u64>, _> =
        version.trim().split('.').map(|p| p.parse::<u64>()).collect();
    parts.ok().filter(|v| !v.is_empty())
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (ver_tuple(latest), ver_tuple(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn http_get(url: &str) -> anyhow::Result<String> {
    let mut res = ureq::get(url)
        .header("User-Agent", UA)
        .call()?;
    Ok(res.body_mut().read_to_string()?)
}

type MapCache = Mutex<Option<(HashMap<String, String>, Instant)>>;

fn map_cache() -> &'static MapCache {
    static CACHE: OnceLock<MapCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

fn official_map(force: bool) -> anyhow::Result<HashMap<String, String>> {
    if !force
        && let Some((map, ts)) = map_cache().lock().unwrap().as_ref()
        && ts.elapsed() < TTL
    {
        return Ok(map.clone());
    }
    let html = http_get(OFFICIAL_URL)?;
    let re = Regex::new(concat!(
        r#"(?s)<script id="__NEXT_DATA__" type="application/json">"#,
        r"(.*?)</script>")).unwrap();
    let raw = re.captures(&html)
        .ok_or_else(|| anyhow::anyhow!("__NEXT_DATA__ not found"))?
        .get(1).unwrap().as_str();
    let data: serde_json::Value = serde_json::from_str(raw)?;
    let pmap = data.pointer("/props/pageProps/printerMap")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow::anyhow!("printerMap missing"))?;
    let mut latest = HashMap::new();
    for (key, entry) in pmap {
        if let Some(version) = entry.pointer("/versions/0/version")
            .and_then(|v| v.as_str())
        {
            latest.insert(key.clone(), version.to_string());
        }
    }
    if latest.is_empty() {
        anyhow::bail!("printerMap empty");
    }
    *map_cache().lock().unwrap() = Some((latest.clone(), Instant::now()));
    Ok(latest)
}

fn mirror_latest(model: &str) -> anyhow::Result<String> {
    let Some(feed) = mirror_feed(model) else {
        return Ok(String::new());
    };
    let body = http_get(&format!("{MIRROR_BASE}{feed}"))?;
    let data: serde_json::Value = serde_json::from_str(&body)?;
    Ok(data.pointer("/upgrade/firmware_optional/firmware/version")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string())
}

/// Blocking: newest released firmware for `model` ("" if unknown).
pub fn fetch_latest(model: &str, force: bool) -> String {
    if let Some(key) = model_key(model)
        && let Ok(map) = official_map(force)
        && let Some(version) = map.get(key)
    {
        return version.clone();
    }
    mirror_latest(model).unwrap_or_default()
}

/// Async check; `slot` is filled with Some(version-or-empty) when done.
pub fn spawn_check(model: String, force: bool,
                   slot: Arc<Mutex<Option<String>>>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let version = fetch_latest(&model, force);
        *slot.lock().unwrap() = Some(version);
        ctx.request_repaint();
    });
}
