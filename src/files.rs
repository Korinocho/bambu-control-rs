//! Fetch job data (plate thumbnail + skippable object list) from the
//! printer's SD card over implicit FTPS :990 (bblp / access code).
//! Port of the Python `core/files.py`.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use regex_lite::Regex;
use suppaftp::native_tls::TlsConnector;
use suppaftp::{NativeTlsConnector, NativeTlsFtpStream};

#[derive(Default, Clone)]
pub struct JobBundle {
    pub plate_png: Option<Vec<u8>>,
    /// (identify_id, label)
    pub objects: Vec<(i64, String)>,
    /// id -> [x1, y1, x2, y2] in mm
    pub bboxes: HashMap<i64, [f32; 4]>,
    pub skipped: HashSet<i64>,
    pub label_objects: bool,
    pub error: String,
}

fn parse_slice_info(xml: &str) -> (Vec<(i64, String)>, HashSet<i64>, bool) {
    let re_obj = Regex::new(r"<object\s+([^>]*?)/>").unwrap();
    let re_attr = Regex::new(r#"(\w+)="([^"]*)""#).unwrap();
    let mut objects = Vec::new();
    let mut skipped = HashSet::new();
    for cap in re_obj.captures_iter(xml) {
        let attrs: HashMap<&str, &str> = re_attr
            .captures_iter(cap.get(1).unwrap().as_str())
            .map(|c| (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str()))
            .collect();
        let Some(iid) = attrs.get("identify_id")
            .and_then(|s| s.parse::<i64>().ok()) else { continue };
        let name = attrs.get("name").filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("object {iid}"));
        objects.push((iid, name));
        if attrs.get("skipped") == Some(&"true") {
            skipped.insert(iid);
        }
    }
    let label = xml.contains(r#"key="label_object_enabled" value="true""#);
    (objects, skipped, label)
}

fn parse_model_settings(xml: &str) -> Vec<(i64, String)> {
    let re_object =
        Regex::new(r#"(?s)<object id="(\d+)"[^>]*>(.*?)</object>"#).unwrap();
    let re_name =
        Regex::new(r#"<metadata key="name" value="([^"]*)""#).unwrap();
    let mut names: HashMap<String, String> = HashMap::new();
    for cap in re_object.captures_iter(xml) {
        let id = cap.get(1).unwrap().as_str().to_string();
        let body = cap.get(2).unwrap().as_str();
        let name = re_name.captures(body)
            .map(|c| c.get(1).unwrap().as_str().to_string())
            .unwrap_or_else(|| format!("object {id}"));
        names.insert(id, name);
    }
    let re_inst =
        Regex::new(r"(?s)<model_instance>(.*?)</model_instance>").unwrap();
    let re_oid =
        Regex::new(r#"<metadata key="object_id" value="(\d+)""#).unwrap();
    let re_iid =
        Regex::new(r#"<metadata key="identify_id" value="(\d+)""#).unwrap();
    let mut objects = Vec::new();
    let mut counts: HashMap<String, u32> = HashMap::new();
    for cap in re_inst.captures_iter(xml) {
        let body = cap.get(1).unwrap().as_str();
        let Some(iid) = re_iid.captures(body)
            .and_then(|c| c.get(1).unwrap().as_str().parse::<i64>().ok())
        else { continue };
        let base = re_oid.captures(body)
            .and_then(|c| names.get(c.get(1).unwrap().as_str()))
            .cloned()
            .unwrap_or_else(|| "object".to_string());
        let n = counts.entry(base.clone()).or_insert(0);
        *n += 1;
        let label = if *n == 1 { base } else { format!("{base} #{n}") };
        objects.push((iid, label));
    }
    objects
}

pub fn fetch_job_bundle(ip: &str, access_code: &str, job_name: &str,
                        file_name: &str,
                        progress: &dyn Fn(u8)) -> JobBundle {
    match fetch_inner(ip, access_code, job_name, file_name, progress) {
        Ok(bundle) => bundle,
        Err(e) => JobBundle { error: e.to_string(), ..Default::default() },
    }
}

fn fetch_inner(ip: &str, access_code: &str, job_name: &str,
               file_name: &str,
               progress: &dyn Fn(u8)) -> anyhow::Result<JobBundle> {
    let mut bundle = JobBundle { label_objects: true, ..Default::default() };

    let tls = TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()?;
    let mut ftp = NativeTlsFtpStream::connect_secure_implicit(
        format!("{ip}:990"), NativeTlsConnector::from(tls), ip)?;
    ftp.login("bblp", access_code)?;
    ftp.transfer_type(suppaftp::types::FileType::Binary)?;

    let mut candidates: Vec<String> = Vec::new();
    for folder in ["/cache", "/"] {
        if let Ok(names) = ftp.nlst(Some(folder)) {
            for name in names {
                if name.to_lowercase().ends_with(".3mf") {
                    let path = if name.starts_with('/') {
                        name
                    } else {
                        format!("{}/{}", folder.trim_end_matches('/'), name)
                    };
                    candidates.push(path);
                }
            }
        }
    }

    let job_lower = job_name.to_lowercase();
    let mut exact: HashSet<String> = HashSet::new();
    if !file_name.is_empty() {
        let fname = file_name.rsplit('/').next().unwrap_or("").to_lowercase();
        if let Some(stem) = fname.strip_suffix(".gcode.3mf") {
            exact.insert(format!("{stem}.3mf"));
        }
        exact.insert(fname);
    }
    exact.insert(format!("{job_lower}.3mf"));

    let basename =
        |p: &str| p.rsplit('/').next().unwrap_or("").to_lowercase();
    let mut target = candidates.iter()
        .find(|p| exact.contains(&basename(p)))
        .cloned();
    if target.is_none() {
        // fuzzy fallback — prefer the longest (most specific) stem
        let mut best: Option<(usize, String)> = None;
        for path in &candidates {
            let stem = basename(path);
            let stem = stem.strip_suffix(".3mf").unwrap_or(&stem).to_string();
            if stem == job_lower || job_lower.contains(&stem)
                || stem.contains(&job_lower)
            {
                if best.as_ref().is_none_or(|(len, _)| stem.len() > *len) {
                    best = Some((stem.len(), path.clone()));
                }
            }
        }
        target = best.map(|(_, p)| p);
    }
    let Some(target) = target else {
        bundle.error = format!("no 3mf matching '{job_name}' on SD");
        return Ok(bundle);
    };

    let total = ftp.size(&target).ok();
    let mut reader = ftp.retr_as_stream(&target)?;
    let mut data: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..n]);
        if let Some(total) = total
            && total > 0
        {
            progress(((data.len() * 100 / total).min(99)) as u8);
        }
    }
    ftp.finalize_retr_stream(reader)?;
    progress(100);
    let _ = ftp.quit();

    let mut zip = zip::ZipArchive::new(Cursor::new(data))?;
    let read_entry = |zip: &mut zip::ZipArchive<Cursor<Vec<u8>>>,
                      name: &str| -> Option<Vec<u8>> {
        let mut file = zip.by_name(name).ok()?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;
        Some(buf)
    };

    for cand in ["Metadata/plate_1.png", "Metadata/top_1.png"] {
        if let Some(png) = read_entry(&mut zip, cand) {
            bundle.plate_png = Some(png);
            break;
        }
    }
    if let Some(xml) = read_entry(&mut zip, "Metadata/slice_info.config") {
        let text = String::from_utf8_lossy(&xml).to_string();
        let (objects, skipped, label) = parse_slice_info(&text);
        bundle.objects = objects;
        bundle.skipped = skipped;
        bundle.label_objects = label;
    }
    if let Some(raw) = read_entry(&mut zip, "Metadata/plate_1.json")
        && let Ok(plate) = serde_json::from_slice::<serde_json::Value>(&raw)
    {
        let bbox_objects = plate.get("bbox_objects")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for obj in &bbox_objects {
            let (Some(id), Some(bbox)) = (
                obj.get("id").and_then(|v| v.as_i64()),
                obj.get("bbox").and_then(|v| v.as_array()),
            ) else { continue };
            if bbox.len() == 4 {
                let mut arr = [0f32; 4];
                for (i, v) in bbox.iter().enumerate() {
                    arr[i] = v.as_f64().unwrap_or(0.0) as f32;
                }
                bundle.bboxes.insert(id, arr);
            }
        }
        // plate json ids are the printer-side identify ids — prefer
        // them whenever present so skip commands match
        if !bbox_objects.is_empty()
            && bbox_objects.len() >= bundle.objects.len()
        {
            let mut counts: HashMap<String, u32> = HashMap::new();
            let mut objs = Vec::new();
            for obj in &bbox_objects {
                let Some(id) = obj.get("id").and_then(|v| v.as_i64())
                else { continue };
                let base = obj.get("name").and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("object {id}"));
                let n = counts.entry(base.clone()).or_insert(0);
                *n += 1;
                let label =
                    if *n == 1 { base } else { format!("{base} #{n}") };
                objs.push((id, label));
            }
            if !objs.is_empty() {
                bundle.objects = objs;
            }
        }
    }
    if bundle.objects.is_empty()
        && let Some(xml) = read_entry(&mut zip, "Metadata/model_settings.config")
    {
        bundle.objects =
            parse_model_settings(&String::from_utf8_lossy(&xml));
    }
    Ok(bundle)
}

/// Spawned fetch with shared progress + result slots for the GUI.
pub struct JobFetch {
    pub result: Arc<Mutex<Option<JobBundle>>>,
    pub progress: Arc<AtomicU8>,
    pub job: String,
}

impl JobFetch {
    pub fn spawn(ip: String, access_code: String, job: String,
                 file_name: String, ctx: egui::Context) -> Arc<Self> {
        let me = Arc::new(Self {
            result: Arc::new(Mutex::new(None)),
            progress: Arc::new(AtomicU8::new(0)),
            job: job.clone(),
        });
        let handle = me.clone();
        std::thread::spawn(move || {
            let progress_cb = {
                let progress = handle.progress.clone();
                let ctx = ctx.clone();
                move |pct: u8| {
                    progress.store(pct, Ordering::Relaxed);
                    ctx.request_repaint();
                }
            };
            let bundle = fetch_job_bundle(
                &ip, &access_code, &job, &file_name, &progress_cb);
            *handle.result.lock().unwrap() = Some(bundle);
            ctx.request_repaint();
        });
        me
    }
}
