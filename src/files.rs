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

    let Some(target) = pick_3mf(&candidates, job_name, file_name) else {
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

    read_3mf(data, job_plate(job_name, file_name))
}

/// Studio shortens long upload names to 97 characters + "...".
const TRUNCATED_STEM_CHARS: usize = 100;

fn file_basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or("").to_lowercase()
}

/// Lowercase name without .gcode.3mf / .3mf / .gcode.
fn name_stem(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    for ext in [".gcode.3mf", ".3mf", ".gcode"] {
        if let Some(stem) = lower.strip_suffix(ext) {
            return stem.trim_end().to_string();
        }
    }
    lower
}

/// The part Studio kept of a shortened name.
fn truncated_prefix(stem: &str) -> Option<String> {
    (stem.chars().count() == TRUNCATED_STEM_CHARS && stem.ends_with("..."))
        .then(|| stem.chars().take(TRUNCATED_STEM_CHARS - 3).collect())
}

/// Picks the job's 3mf: the file gcode_file names, else the one with the
/// job's name or Studio's shortened form of it. A wrong file means skip
/// commands for the wrong objects, so a loose match is no match.
fn pick_3mf(candidates: &[String], job_name: &str,
            file_name: &str) -> Option<String> {
    // root uploads first: where both exist the /cache copy was the older
    let in_cache = |p: &&String| p.to_lowercase().starts_with("/cache/");
    let ordered: Vec<&String> = candidates.iter()
        .filter(|p| !in_cache(p))
        .chain(candidates.iter().filter(in_cache))
        .collect();

    let fname = file_basename(file_name);
    let mut exact: Vec<String> = Vec::new();
    if !fname.is_empty() {
        exact.push(fname.clone());
        if let Some(stem) = fname.strip_suffix(".gcode.3mf")
            && !stem.is_empty()
        {
            exact.push(format!("{stem}.3mf"));
        }
    }
    for name in &exact {
        if let Some(path) = ordered.iter().find(|p| file_basename(p) == *name)
        {
            return Some((*path).clone());
        }
    }

    // a job name is a project name, not a path: the SD copy has '/' as '_'
    let job = if job_name == file_name {
        name_stem(&file_basename(job_name))
    } else {
        name_stem(&job_name.replace(['/', '\\'], "_"))
    };
    if job.is_empty() {
        return None;
    }
    // (rank, matched length, path): rank 0 = same stem, 1 = shortened name
    let mut hits: Vec<(u8, usize, &String)> = Vec::new();
    for path in ordered {
        let stem = name_stem(&file_basename(path));
        if stem.is_empty() {
            continue;
        }
        if stem == job {
            hits.push((0, stem.len(), path));
        } else if let Some(prefix) = truncated_prefix(&stem)
            && job.starts_with(&prefix)
        {
            hits.push((1, prefix.len(), path));
        }
    }
    hits.sort_by_key(|(rank, len, _)| (*rank, std::cmp::Reverse(*len)));
    hits.first().map(|(_, _, path)| (*path).clone())
}

/// Plate number in the reported gcode_file ("/data/Metadata/plate_3.gcode")
/// or job name ("X_plate_3").
fn job_plate(job_name: &str, file_name: &str) -> Option<u32> {
    let re = Regex::new(r"plate_(\d+)(?:\.gcode)?$").unwrap();
    [file_name, job_name].iter().find_map(|name| {
        re.captures(&name.trim().to_lowercase())
            .and_then(|c| c.get(1).unwrap().as_str().parse().ok())
    })
}

/// Plate the job was sliced for, None when the file can't tell. A job 3mf
/// carries only the printed plate's gcode; slice_info has one `<plate>`
/// block per sliced plate; the reported plate number breaks ties.
fn sliced_plate(names: &[String], slice_info: Option<&str>,
                reported: Option<u32>) -> Option<u32> {
    let re_gcode = Regex::new(r"^Metadata/plate_(\d+)\.gcode$").unwrap();
    let mut plates: Vec<u32> = names.iter()
        .filter_map(|n| re_gcode.captures(n))
        .filter_map(|c| c.get(1).unwrap().as_str().parse().ok())
        .collect();
    plates.sort_unstable();
    plates.dedup();
    let re_index =
        Regex::new(r#"<metadata key="index" value="(\d+)""#).unwrap();
    let indices: Vec<u32> = slice_info
        .map(|xml| re_index.captures_iter(xml)
            .filter_map(|c| c.get(1).unwrap().as_str().parse().ok())
            .collect())
        .unwrap_or_default();
    if let Some(n) = reported
        && (plates.contains(&n)
            || (plates.is_empty()
                && (indices.is_empty() || indices.contains(&n))))
    {
        return Some(n);
    }
    match plates.as_slice() {
        [only] => Some(*only),
        [] => match indices.as_slice() {
            [only] => Some(*only),
            _ => None,
        },
        _ => {
            let mut sliced = indices.iter().filter(|i| plates.contains(i));
            match (sliced.next(), sliced.next()) {
                (Some(&only), None) => Some(only),
                _ => None,
            }
        }
    }
}

/// slice_info's `<plate>` block for `plate`; a lone block without an index
/// also counts.
fn plate_block(xml: &str, plate: Option<u32>) -> Option<&str> {
    let blocks: Vec<&str> = xml.split("<plate>").skip(1).collect();
    if let Some(n) = plate
        && let Some(block) = blocks.iter()
            .find(|b| b.contains(&format!(r#"key="index" value="{n}""#)))
    {
        return Some(*block);
    }
    match blocks.as_slice() {
        [only] if plate.is_none() || !only.contains(r#"key="index""#) => {
            Some(*only)
        }
        _ => None,
    }
}

/// Skip commands need the printer's object ids (slice_info identify_id,
/// the gcode's OBJECT_ID); plate json uses other ids. So a box goes to the
/// object with the same name, only where that name is unique on both
/// sides — look-alike instances get no box rather than a wrong one.
fn bboxes_by_name(plate_json: &serde_json::Value,
                  objects: &[(i64, String)]) -> HashMap<i64, [f32; 4]> {
    let mut boxes: HashMap<&str, Option<[f32; 4]>> = HashMap::new();
    let entries = plate_json.get("bbox_objects").and_then(|v| v.as_array());
    for obj in entries.into_iter().flatten() {
        let Some(name) = obj.get("name").and_then(|v| v.as_str())
        else { continue };
        let bbox = obj.get("bbox").and_then(|v| v.as_array())
            .filter(|b| b.len() == 4)
            .map(|b| {
                let mut arr = [0f32; 4];
                for (i, v) in b.iter().enumerate() {
                    arr[i] = v.as_f64().unwrap_or(0.0) as f32;
                }
                arr
            });
        boxes.entry(name).and_modify(|b| *b = None).or_insert(bbox);
    }
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for (_, name) in objects {
        *counts.entry(name.as_str()).or_insert(0) += 1;
    }
    objects.iter()
        .filter(|(_, name)| counts[name.as_str()] == 1)
        .filter_map(|(id, name)| {
            Some((*id, boxes.get(name.as_str()).copied().flatten()?))
        })
        .collect()
}

fn read_3mf(data: Vec<u8>, reported_plate: Option<u32>)
            -> anyhow::Result<JobBundle> {
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
    let plate = sliced_plate(&names, slice_info.as_deref(), reported_plate);

    // this plate's images only — another plate's picture would mislead
    if let Some(plate) = plate {
        for cand in [format!("Metadata/plate_{plate}.png"),
                     format!("Metadata/top_{plate}.png")] {
            if let Some(png) = read_entry(&mut zip, &cand) {
                bundle.plate_png = Some(png);
                break;
            }
        }
    }
    if let Some(block) = slice_info.as_deref()
        .and_then(|xml| plate_block(xml, plate))
    {
        let (objects, skipped, label) = parse_slice_info(block);
        bundle.objects = objects;
        bundle.skipped = skipped;
        bundle.label_objects = label;
    }
    if bundle.objects.is_empty()
        && let Some(xml) = read_entry(&mut zip, "Metadata/model_settings.config")
    {
        bundle.objects =
            parse_model_settings(&String::from_utf8_lossy(&xml));
    }
    if let Some(plate) = plate
        && let Some(raw) =
            read_entry(&mut zip, &format!("Metadata/plate_{plate}.json"))
        && let Ok(plate_json) =
            serde_json::from_slice::<serde_json::Value>(&raw)
    {
        bundle.bboxes = bboxes_by_name(&plate_json, &bundle.objects);
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

    use super::{job_plate, read_3mf, sliced_plate};

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

    /// slice_info with one `<plate>` block per index: (index, id, name).
    fn slice_info(objects: &[(u32, i64, &str)]) -> String {
        let mut xml = String::from("<config>");
        let mut open = None;
        for (index, iid, name) in objects {
            if open != Some(*index) {
                if open.is_some() {
                    xml += "</plate>\n";
                }
                xml += &format!("<plate>\n  \
                    <metadata key=\"index\" value=\"{index}\"/>\n");
                open = Some(*index);
            }
            xml += &format!("  <object identify_id=\"{iid}\" \
                name=\"{name}\" skipped=\"false\" />\n");
        }
        if open.is_some() {
            xml += "</plate>\n";
        }
        xml + "</config>"
    }

    fn plate_json(objects: &[(i64, &str)]) -> String {
        let items: Vec<String> = objects.iter()
            .map(|(id, name)| format!(
                r#"{{"id":{id},"name":"{name}","bbox":[1.0,2.0,3.0,4.0]}}"#))
            .collect();
        format!(r#"{{"bbox_objects":[{}]}}"#, items.join(","))
    }

    fn pick(candidates: &[&str], job: &str, file: &str) -> Option<String> {
        let candidates: Vec<String> =
            candidates.iter().map(|s| s.to_string()).collect();
        super::pick_3mf(&candidates, job, file)
    }

    /// Studio's shortened upload name: the first 97 characters + "...".
    fn shortened(name: &str) -> String {
        let kept: String = name.chars().take(97).collect();
        format!("{kept}...")
    }

    #[test]
    fn plate_n_job_uses_its_own_thumbnail() {
        // Studio keeps every plate's pictures but only the sliced plate's
        // gcode + json (seen on an A1 job sliced as plate 2 only)
        let data = make_3mf(&[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/plate_2.png", b"plate-2"),
            ("Metadata/plate_2.gcode", b"; gcode"),
            ("Metadata/plate_2.gcode.md5", b"0"),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.plate_png.as_deref(), Some(&b"plate-2"[..]));
    }

    #[test]
    fn skip_ids_are_slice_info_identify_ids() {
        // ids from the A1 #1 job: its gcode labels the object
        // "; OBJECT_ID: 484" while plate_2.json calls it 506
        let info = slice_info(&[(2, 484, "Soporte.stl_3")]);
        let json = plate_json(&[(506, "Soporte.stl_3")]);
        let data = make_3mf(&[
            ("Metadata/plate_2.png", b"plate-2"),
            ("Metadata/plate_2.gcode", b"; gcode"),
            ("Metadata/plate_2.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.objects, vec![(484, "Soporte.stl_3".to_string())]);
        assert_eq!(bundle.bboxes.get(&484), Some(&[1.0, 2.0, 3.0, 4.0]));
        assert!(!bundle.bboxes.contains_key(&506));
    }

    #[test]
    fn single_plate_job_reads_plate_1() {
        let info = slice_info(&[(1, 91, "Square.stl")]);
        let json = plate_json(&[(107, "Square.stl")]);
        let data = make_3mf(&[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_1.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.plate_png.as_deref(), Some(&b"plate-1"[..]));
        assert_eq!(bundle.objects, vec![(91, "Square.stl".to_string())]);
        assert!(bundle.bboxes.contains_key(&91));
    }

    #[test]
    fn lookalike_instances_get_no_box() {
        let info = slice_info(&[(1, 11, "part"), (1, 12, "part"),
                                (1, 13, "lid")]);
        let json = plate_json(&[(20, "part"), (21, "part"), (22, "lid")]);
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_1.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.objects.len(), 3);
        let boxed: Vec<i64> = bundle.bboxes.keys().copied().collect();
        assert_eq!(boxed, vec![13]);
    }

    #[test]
    fn reported_plate_picks_among_several() {
        let info = slice_info(&[(1, 10, "p1obj"), (3, 30, "p3obj")]);
        let entries: &[(&str, &[u8])] = &[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/plate_3.png", b"plate-3"),
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_3.gcode", b"; gcode"),
            ("Metadata/slice_info.config", info.as_bytes()),
        ];
        let bundle = read_3mf(make_3mf(entries), Some(3)).unwrap();
        assert_eq!(bundle.plate_png.as_deref(), Some(&b"plate-3"[..]));
        assert_eq!(bundle.objects, vec![(30, "p3obj".to_string())]);
        // without a reported plate the file can't tell: show nothing
        let bundle = read_3mf(make_3mf(entries), None).unwrap();
        assert_eq!(bundle.plate_png, None);
        assert!(bundle.objects.is_empty());
    }

    #[test]
    fn lone_plate_block_without_index_still_lists_objects() {
        let info = "<config><plate>\n  \
            <object identify_id=\"5\" name=\"a\" skipped=\"false\" />\n\
            </plate></config>";
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.objects, vec![(5, "a".to_string())]);
    }

    #[test]
    fn falls_back_to_top_view_of_the_same_plate() {
        let data = make_3mf(&[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/top_2.png", b"top-2"),
            ("Metadata/plate_2.gcode", b"; gcode"),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.plate_png.as_deref(), Some(&b"top-2"[..]));
    }

    #[test]
    fn never_shows_another_plates_image() {
        let data = make_3mf(&[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/plate_2.gcode", b"; gcode"),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.plate_png, None);
        assert!(bundle.bboxes.is_empty());
    }

    #[test]
    fn sliced_plate_rules() {
        let names = |list: &[&str]| -> Vec<String> {
            list.iter().map(|s| s.to_string()).collect()
        };
        let plate_2 = slice_info(&[(2, 1, "a")]);
        let plate_3 = slice_info(&[(3, 2, "b")]);
        let plates_1_3 = slice_info(&[(1, 1, "a"), (3, 2, "b")]);
        let both = names(&["Metadata/plate_1.gcode",
                           "Metadata/plate_3.gcode"]);
        // the file's only gcode entry beats a different reported plate
        assert_eq!(sliced_plate(&names(&["Metadata/plate_2.gcode"]),
                                Some(&plate_2), Some(3)), Some(2));
        // several sliced plates: the reported one, or the one slice_info
        // agrees on, else no guess
        assert_eq!(sliced_plate(&both, Some(&plates_1_3), Some(3)), Some(3));
        assert_eq!(sliced_plate(&both, Some(&plate_3), None), Some(3));
        assert_eq!(sliced_plate(&both, Some(&plates_1_3), None), None);
        // md5 sidecars are not gcode entries
        assert_eq!(sliced_plate(&names(&["Metadata/plate_4.gcode.md5"]),
                                Some(&plate_3), None), Some(3));
        // no gcode: the reported plate, else nothing to go on
        assert_eq!(sliced_plate(&names(&[]), None, Some(2)), Some(2));
        assert_eq!(sliced_plate(&names(&[]), None, None), None);
        assert_eq!(sliced_plate(&names(&["Metadata/plate_99999999999.gcode"]),
                                None, None), None);
    }

    #[test]
    fn job_plate_reads_gcode_file_then_job_name() {
        assert_eq!(job_plate("X_plate_3", ""), Some(3));
        assert_eq!(job_plate("X", "/data/Metadata/plate_2.gcode"), Some(2));
        assert_eq!(job_plate("X_plate_3", "/data/Metadata/plate_2.gcode"),
                   Some(2));
        assert_eq!(job_plate("X", "X.gcode.3mf"), None);
    }

    #[test]
    fn printer_reported_file_beats_same_named_cache_copy() {
        let found = pick(&["/cache/part.3mf", "/part.gcode.3mf"],
                         "part", "part.gcode.3mf");
        assert_eq!(found.as_deref(), Some("/part.gcode.3mf"));
    }

    #[test]
    fn root_upload_beats_cache_copy_without_gcode_file() {
        // gcode_file is empty once a print ends; on the owner's cards the
        // root upload was the newer copy in every such pair
        let found = pick(&["/cache/(Unsaved).3mf", "/(Unsaved).gcode.3mf"],
                         "(Unsaved)", "");
        assert_eq!(found.as_deref(), Some("/(Unsaved).gcode.3mf"));
        let found = pick(&["/cache/part.3mf", "/part.3mf"], "x", "part.3mf");
        assert_eq!(found.as_deref(), Some("/part.3mf"));
    }

    #[test]
    fn gcode_3mf_name_finds_cache_3mf() {
        let found = pick(&["/cache/part.3mf"], "", "part.gcode.3mf");
        assert_eq!(found.as_deref(), Some("/cache/part.3mf"));
        // an empty stem in gcode_file names nothing
        assert_eq!(pick(&["/cache/.3mf"], "", ".gcode.3mf"), None);
    }

    #[test]
    fn job_name_matches_regardless_of_extension_case_and_spaces() {
        let found = pick(&["/First Layer Square.stl.gcode.3mf"],
                         "first layer square.stl ", "");
        assert_eq!(found.as_deref(),
                   Some("/First Layer Square.stl.gcode.3mf"));
        // job taken from a gcode_file path
        let found = pick(&["/cache/part.3mf"], "/sdcard/part.gcode",
                         "/sdcard/part.gcode");
        assert_eq!(found.as_deref(), Some("/cache/part.3mf"));
    }

    #[test]
    fn slash_in_job_name_is_not_a_path() {
        // Studio stores "/" in upload names as "_"
        let cache = ["/cache/18.7mm Hole (for 1_2 inch EMT).3mf",
                     "/cache/Base.3mf"];
        let found = pick(&cache, "18.7mm Hole (for 1/2 inch EMT)", "");
        assert_eq!(found.as_deref(),
                   Some("/cache/18.7mm Hole (for 1_2 inch EMT).3mf"));
        assert_eq!(pick(&cache, "Tool holder/Base", ""), None);
    }

    #[test]
    fn plate_job_never_takes_the_base_project() {
        // a 3mf holds one plate: X.gcode.3mf is another upload than the
        // X_plate_2 job (seen on the P1S with the plate_2 file deleted)
        assert_eq!(pick(&["/Fidget+Cube+Toy-Sofi.gcode.3mf"],
                        "Fidget+Cube+Toy-Sofi_plate_2", ""), None);
        let found = pick(&["/Fidget+Cube+Toy-Sofi.gcode.3mf",
                           "/Fidget+Cube+Toy-Sofi_plate_2.gcode.3mf"],
                         "Fidget+Cube+Toy-Sofi_plate_2", "");
        assert_eq!(found.as_deref(),
                   Some("/Fidget+Cube+Toy-Sofi_plate_2.gcode.3mf"));
    }

    #[test]
    fn empty_stem_matches_nothing() {
        // the old fallback matched a file named ".3mf" to every job
        assert_eq!(pick(&["/cache/.3mf"], "anything", ""), None);
    }

    #[test]
    fn substrings_are_not_matches() {
        let cache = ["/cache/cap.3mf", "/cache/cube.3mf",
                     "/cache/bracket_v2_final.3mf"];
        assert_eq!(pick(&cache, "escape_key_cap_v2", ""), None);
        assert_eq!(pick(&cache, "big cube", ""), None);
        assert_eq!(pick(&cache, "bracket", ""), None);
    }

    #[test]
    fn shortened_upload_name_matches_full_job_name() {
        let job = "Fidget+Cube+toy+.stl + Fidget+Cube+toy+.stl 1 + \
                   Fidget+Cube+toy+.stl 2 + Fidget+Cube+toy+.stl 3 + \
                   Fidget+Cube+toy+.stl 4";
        let file = format!("/{}.gcode.3mf", shortened(job));
        assert_eq!(pick(&[file.as_str()], job, "").as_deref(),
                   Some(file.as_str()));
        // the kept part has to start the job, not appear inside it
        assert_eq!(pick(&[file.as_str()], &format!("x {job}"), ""), None);
        // a short name that merely ends in "..." is not a shortened one
        assert_eq!(pick(&["/cache/Wait for it....3mf"], "wait for it v2",
                        ""), None);
    }

    #[test]
    fn same_stem_beats_a_shortened_name_of_equal_length() {
        // a 97-character job equals the kept part of a longer job's
        // shortened name; its own file wins even from /cache
        let job = "a".repeat(90) + " part 1";
        let other = format!("/{}.gcode.3mf",
                            shortened(&format!("{job} + more")));
        let own = format!("/cache/{job}.3mf");
        let found = pick(&[own.as_str(), other.as_str()], &job, "");
        assert_eq!(found.as_deref(), Some(own.as_str()));
    }

    #[test]
    fn no_candidates_no_match() {
        assert_eq!(pick(&[], "part", "part.gcode.3mf"), None);
    }
}
