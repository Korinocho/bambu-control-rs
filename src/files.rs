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

    read_3mf(data)
}

/// Plate the job was sliced for. A job 3mf only carries the printed
/// plate's gcode, so that entry wins; slice_info's index is the fallback.
fn sliced_plate(names: &[String], slice_info: Option<&str>) -> u32 {
    let re_gcode = Regex::new(r"^Metadata/plate_(\d+)\.gcode$").unwrap();
    let mut plates: Vec<u32> = names.iter()
        .filter_map(|n| re_gcode.captures(n))
        .filter_map(|c| c.get(1).unwrap().as_str().parse().ok())
        .collect();
    plates.sort_unstable();
    let index = slice_info.and_then(|xml| {
        Regex::new(r#"<metadata key="index" value="(\d+)""#).unwrap()
            .captures(xml)
            .and_then(|c| c.get(1).unwrap().as_str().parse::<u32>().ok())
    });
    match (plates.first(), index) {
        (Some(_), Some(i)) if plates.contains(&i) => i,
        (Some(&first), _) => first,
        (None, Some(i)) => i,
        (None, None) => 1,
    }
}

fn read_3mf(data: Vec<u8>) -> anyhow::Result<JobBundle> {
    let mut bundle = JobBundle { label_objects: true, ..Default::default() };
    let mut zip = zip::ZipArchive::new(Cursor::new(data))?;
    let read_entry = |zip: &mut zip::ZipArchive<Cursor<Vec<u8>>>,
                      name: &str| -> Option<Vec<u8>> {
        let mut file = zip.by_name(name).ok()?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;
        Some(buf)
    };

    let names: Vec<String> = zip.file_names().map(str::to_string).collect();
    let slice_info = read_entry(&mut zip, "Metadata/slice_info.config")
        .map(|xml| String::from_utf8_lossy(&xml).to_string());
    let plate = sliced_plate(&names, slice_info.as_deref());

    // this plate's images only — another plate's picture would mislead
    for cand in [format!("Metadata/plate_{plate}.png"),
                 format!("Metadata/top_{plate}.png")] {
        if let Some(png) = read_entry(&mut zip, &cand) {
            bundle.plate_png = Some(png);
            break;
        }
    }
    if let Some(text) = &slice_info {
        let (objects, skipped, label) = parse_slice_info(text);
        bundle.objects = objects;
        bundle.skipped = skipped;
        bundle.label_objects = label;
    }
    let json_name = format!("Metadata/plate_{plate}.json");
    if let Some(raw) = read_entry(&mut zip, &json_name)
        && let Ok(plate_json) =
            serde_json::from_slice::<serde_json::Value>(&raw)
    {
        let bbox_objects = plate_json.get("bbox_objects")
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

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;

    use super::{read_3mf, sliced_plate};

    fn make_3mf(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, body) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn slice_info(index: u32, iid: i64) -> String {
        format!(r#"<config><plate>
  <metadata key="index" value="{index}"/>
  <object identify_id="{iid}" name="part" skipped="false" />
</plate></config>"#)
    }

    fn plate_json(id: i64) -> String {
        format!(r#"{{"bbox_objects":[{{"id":{id},"name":"part",
            "bbox":[1.0,2.0,3.0,4.0]}}]}}"#)
    }

    #[test]
    fn plate_n_job_uses_its_own_thumbnail_and_bboxes() {
        // Studio keeps every plate's pictures but only the sliced plate's
        // gcode + json (seen on an A1 job sliced as plate 2 only)
        let info = slice_info(2, 506);
        let json = plate_json(506);
        let data = make_3mf(&[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/plate_2.png", b"plate-2"),
            ("Metadata/plate_2.gcode", b"; gcode"),
            ("Metadata/plate_2.gcode.md5", b"0"),
            ("Metadata/plate_2.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data).unwrap();
        assert_eq!(bundle.plate_png.as_deref(), Some(&b"plate-2"[..]));
        assert_eq!(bundle.bboxes.get(&506), Some(&[1.0, 2.0, 3.0, 4.0]));
        assert_eq!(bundle.objects, vec![(506, "part".to_string())]);
    }

    #[test]
    fn single_plate_job_reads_plate_1() {
        let info = slice_info(1, 7);
        let json = plate_json(7);
        let data = make_3mf(&[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_1.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data).unwrap();
        assert_eq!(bundle.plate_png.as_deref(), Some(&b"plate-1"[..]));
        assert!(bundle.bboxes.contains_key(&7));
    }

    #[test]
    fn falls_back_to_top_view_of_the_same_plate() {
        let data = make_3mf(&[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/top_2.png", b"top-2"),
            ("Metadata/plate_2.gcode", b"; gcode"),
        ]);
        let bundle = read_3mf(data).unwrap();
        assert_eq!(bundle.plate_png.as_deref(), Some(&b"top-2"[..]));
    }

    #[test]
    fn never_shows_another_plates_image() {
        let data = make_3mf(&[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/plate_2.gcode", b"; gcode"),
        ]);
        let bundle = read_3mf(data).unwrap();
        assert_eq!(bundle.plate_png, None);
        assert!(bundle.bboxes.is_empty());
    }

    #[test]
    fn sliced_plate_prefers_gcode_entry_then_slice_info() {
        let names = |list: &[&str]| -> Vec<String> {
            list.iter().map(|s| s.to_string()).collect()
        };
        let info = slice_info(3, 1);
        // the gcode entry wins over a disagreeing index
        assert_eq!(sliced_plate(&names(&["Metadata/plate_2.gcode"]),
                                Some(&info)), 2);
        // index picks among several gcode entries
        assert_eq!(sliced_plate(&names(&["Metadata/plate_1.gcode",
                                         "Metadata/plate_3.gcode"]),
                                Some(&info)), 3);
        // md5 sidecars are not gcode entries
        assert_eq!(sliced_plate(&names(&["Metadata/plate_4.gcode.md5"]),
                                Some(&info)), 3);
        assert_eq!(sliced_plate(&names(&[]), None), 1);
    }
}
