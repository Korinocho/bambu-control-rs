//! Job data (plate thumbnail + skippable object list) of a printer job: the
//! matcher that picks the job's 3mf on the SD card and the reader that pulls
//! the plate picture, the objects and their boxes out of it. Port of the
//! Python `core/files.py`.
//!
//! The FTPS session code lives in src/ftp.rs and the worker that drives it
//! in src/browser.rs (design doc 5.9), so nothing here touches the network.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};

use regex_lite::Regex;

#[derive(Default, Clone)]
pub struct JobBundle {
    pub plate_png: Option<Vec<u8>>,
    /// (identify_id, label)
    pub objects: Vec<(i64, String)>,
    /// id -> [x1, y1, x2, y2], y up, on a 256-unit bed (mm on 256 mm beds):
    /// from the pick image, else plate json first-layer footprints
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
            .map(|s| xml_unescape(s))
            .unwrap_or_else(|| format!("object {iid}"));
        objects.push((iid, name));
        if attrs.get("skipped") == Some(&"true") {
            skipped.insert(iid);
        }
    }
    let label = xml.contains(r#"key="label_object_enabled" value="true""#);
    (objects, skipped, label)
}

/// Objects of one plate from model_settings, the fallback for a 3mf
/// without slice_info: that plate's model instances, named by object.
fn parse_model_settings(xml: &str, plate: u32) -> Vec<(i64, String)> {
    let re_object =
        Regex::new(r#"(?s)<object id="(\d+)"[^>]*>(.*?)</object>"#).unwrap();
    let re_name =
        Regex::new(r#"<metadata key="name" value="([^"]*)""#).unwrap();
    let mut names: HashMap<String, String> = HashMap::new();
    for cap in re_object.captures_iter(xml) {
        let id = cap.get(1).unwrap().as_str().to_string();
        let body = cap.get(2).unwrap().as_str();
        let name = re_name.captures(body)
            .map(|c| xml_unescape(c.get(1).unwrap().as_str()))
            .unwrap_or_else(|| format!("object {id}"));
        names.insert(id, name);
    }
    let re_plater =
        Regex::new(r#"<metadata key="plater_id" value="(\d+)""#).unwrap();
    let Some(block) = xml.split("<plate>").skip(1).find(|b| {
        re_plater.captures(b)
            .and_then(|c| c.get(1).unwrap().as_str().parse::<u32>().ok())
            == Some(plate)
    }) else { return Vec::new() };
    let re_inst =
        Regex::new(r"(?s)<model_instance>(.*?)</model_instance>").unwrap();
    let re_oid =
        Regex::new(r#"<metadata key="object_id" value="(\d+)""#).unwrap();
    let re_iid =
        Regex::new(r#"<metadata key="identify_id" value="(\d+)""#).unwrap();
    let mut objects = Vec::new();
    for cap in re_inst.captures_iter(block) {
        let body = cap.get(1).unwrap().as_str();
        let Some(iid) = re_iid.captures(body)
            .and_then(|c| c.get(1).unwrap().as_str().parse::<i64>().ok())
        else { continue };
        let name = re_oid.captures(body)
            .and_then(|c| names.get(c.get(1).unwrap().as_str()))
            .cloned()
            .unwrap_or_else(|| format!("object {iid}"));
        objects.push((iid, name));
    }
    objects
}

/// Undoes Studio's XML escaping of names: &amp; &apos; &quot; &lt; &gt;
/// and numeric references. Anything else is kept as written.
fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let decoded = tail.find(';').and_then(|end| {
            let c = match &tail[1..end] {
                "amp" => Some('&'),
                "apos" => Some('\''),
                "quot" => Some('"'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                name => name.strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .map(|hex| {
                        let digits = hex.bytes().all(|b| b.is_ascii_hexdigit());
                        u32::from_str_radix(hex, 16).ok().filter(|_| digits)
                    })
                    .unwrap_or_else(|| {
                        name.strip_prefix('#')
                            .filter(|d| d.bytes().all(|b| b.is_ascii_digit()))
                            .and_then(|d| d.parse().ok())
                    })
                    .and_then(char::from_u32)
                    .filter(|c| *c != '\0'),
            };
            c.map(|c| (c, end))
        });
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Copies share a name: "part", "part" -> "part", "part #2", skipping any
/// label another object already has.
fn label_copies(objects: &mut [(i64, String)]) {
    let mut taken: HashSet<String> =
        objects.iter().map(|(_, name)| name.clone()).collect();
    let mut seen: HashSet<String> = HashSet::new();
    for (_, name) in objects.iter_mut() {
        if seen.insert(name.clone()) {
            continue;
        }
        let label = (2u32..).map(|k| format!("{name} #{k}"))
            .find(|label| !taken.contains(label))
            .unwrap_or_else(|| name.clone());
        taken.insert(label.clone());
        *name = label;
    }
}

/// Studio shortens long upload names to 97 characters + "..." (all 17 on
/// the owner's cards); a 100-byte form is accepted for non-ASCII names.
const TRUNCATED_STEM_LEN: usize = 100;

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
fn truncated_prefix(stem: &str) -> Option<&str> {
    let long = stem.chars().count() == TRUNCATED_STEM_LEN
        || stem.len() == TRUNCATED_STEM_LEN;
    if long { stem.strip_suffix("...") } else { None }
}

/// Characters Studio keeps out of upload names; the cards show them as '_'.
const ILLEGAL_NAME_CHARS: &str = "<>:/\\|?*\"";

/// A job name as the card stores it: illegal characters become '_'.
fn card_form(name: &str) -> String {
    name.chars()
        .map(|c| if ILLEGAL_NAME_CHARS.contains(c) { '_' } else { c })
        .collect()
}

/// Studio's form of a MakerWorld title: spaces, [ ] and illegal characters
/// become '_', runs of '_' collapse; a trailing '_' is dropped.
fn maker_form(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        let mapped = c == ' ' || c == '[' || c == ']'
            || ILLEGAL_NAME_CHARS.contains(c);
        let c = if mapped { '_' } else { c };
        if !(c == '_' && out.ends_with('_')) {
            out.push(c);
        }
    }
    out.trim_end_matches('_').to_string()
}

/// Picks the job's 3mf: the file gcode_file names, else the one named like
/// the job (as is, as Studio sanitises it, or shortened). X.3mf is never
/// taken for an X_plate_N job, since a job 3mf holds one plate. A wrong
/// file means skip commands for the wrong objects: loose is no match.
pub fn pick_3mf(candidates: &[String], job_name: &str, file_name: &str,
                print_type: &str) -> Option<String> {
    // cloud jobs are kept in /cache, LAN uploads in the root
    let in_cache = |p: &&String| p.to_lowercase().starts_with("/cache/");
    let cloud = print_type.eq_ignore_ascii_case("cloud");
    let ordered: Vec<&String> = candidates.iter()
        .filter(|p| in_cache(p) == cloud)
        .chain(candidates.iter().filter(|p| in_cache(p) != cloud))
        .collect();

    let fname = file_basename(file_name);
    if !fname.is_empty()
        && let Some(path) = ordered.iter().find(|p| file_basename(p) == fname)
    {
        return Some((*path).clone());
    }

    // with no subtask name the job is gcode_file, and an X1 ramdisk path
    // ("/data/Metadata/plate_1.gcode") names no project
    let from_file = job_name == file_name;
    let job = name_stem(if from_file { fname.as_str() } else { job_name });
    if job.is_empty()
        || (from_file
            && Regex::new(r"^plate_\d+\.gcode$").unwrap().is_match(&fname))
    {
        return None;
    }
    let job_card = card_form(&job);
    let job_maker = maker_form(&job);
    // rank 0: same stem; 1: the job as the card or a MakerWorld title stores
    // it (one way only: Studio never turns '_' into a space); 2: a shortened
    // name; equal ranks keep the folder order above
    let rank = |path: &&String| -> Option<u8> {
        let stem = name_stem(&file_basename(path));
        let card = stem == job_card && job_card.chars().any(|c| c != '_');
        let maker = !job_maker.is_empty() && !stem.contains(' ')
            && !stem.contains("__") && stem.trim_end_matches('_') == job_maker;
        if stem.is_empty() {
            None
        } else if stem == job {
            Some(0)
        } else if card || maker {
            Some(1)
        } else if truncated_prefix(&stem)
            .is_some_and(|p| job.starts_with(p) || job_card.starts_with(p))
        {
            Some(2)
        } else {
            None
        }
    };
    ordered.iter()
        .filter_map(|p| rank(p).map(|r| (r, *p)))
        .min_by_key(|(r, _)| *r)
        .map(|(_, p)| p.clone())
}

/// Plate number in the reported gcode_file ("/data/Metadata/plate_3.gcode")
/// or job name ("X_plate_3").
pub fn job_plate(job_name: &str, file_name: &str) -> Option<u32> {
    let re = Regex::new(r"(?:^|[/_])plate_(\d+)(?:\.gcode)?$").unwrap();
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
    let re_index =
        Regex::new(r#"<metadata key="index" value="(\d+)""#).unwrap();
    let blocks: Vec<(&str, Option<u32>)> = xml.split("<plate>").skip(1)
        .map(|b| (b, re_index.captures(b)
            .and_then(|c| c.get(1).unwrap().as_str().parse().ok())))
        .collect();
    if let Some(n) = plate
        && let Some((block, _)) = blocks.iter().find(|(_, i)| *i == Some(n))
    {
        return Some(*block);
    }
    match blocks.as_slice() {
        [(only, index)] if plate.is_none() || index.is_none() => Some(*only),
        _ => None,
    }
}

/// Object boxes from Metadata/pick_N.png, where Studio renders the whole
/// bed top-down with each object filled in its identify_id as the colour
/// (R | G << 8 | B << 16). Pixel boxes are scaled to a 256-unit bed, y up:
/// millimetres on 256 mm beds (within 0.5 mm of the plate json on the
/// owner's jobs), not on the A1 mini or H2 beds.
fn pick_bboxes(png: &[u8], ids: &HashSet<i64>) -> HashMap<i64, [f32; 4]> {
    let Ok(img) =
        image::load_from_memory_with_format(png, image::ImageFormat::Png)
    else { return HashMap::new() };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut px: HashMap<i64, [u32; 4]> = HashMap::new();
    for (x, y, p) in rgba.enumerate_pixels() {
        let id = i64::from(p[0]) | i64::from(p[1]) << 8
            | i64::from(p[2]) << 16;
        if p[3] != 255 || !ids.contains(&id) {
            continue;
        }
        let b = px.entry(id).or_insert([x, y, x, y]);
        *b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
    }
    let (sx, sy) = (256.0 / w as f32, 256.0 / h as f32);
    px.into_iter()
        .map(|(id, [x0, y0, x1, y1])| (id, [
            x0 as f32 * sx, (h - 1 - y1) as f32 * sy,
            (x1 + 1) as f32 * sx, (h - y0) as f32 * sy]))
        .collect()
}

/// Fallback without a pick image. Plate json uses other ids than the
/// printer (slice_info identify_id, the gcode's OBJECT_ID), so a box goes
/// to the object with the same name, only where that name is unique on
/// both sides — look-alike copies get no box rather than a wrong one.
fn bboxes_by_name(plate_json: &serde_json::Value,
                  objects: &[(i64, String)]) -> HashMap<i64, [f32; 4]> {
    let mut boxes: HashMap<&str, Option<[f32; 4]>> = HashMap::new();
    let entries = plate_json.get("bbox_objects").and_then(|v| v.as_array());
    for obj in entries.into_iter().flatten() {
        let Some(name) = obj.get("name").and_then(|v| v.as_str())
        else { continue };
        let bbox = obj.get("bbox").and_then(|v| v.as_array())
            .and_then(|b| <&[serde_json::Value; 4]>::try_from(b.as_slice())
                .ok())
            .map(|b| b.each_ref().map(|v| v.as_f64().unwrap_or(0.0) as f32));
        boxes.entry(name).and_modify(|b| *b = None).or_insert(bbox);
    }
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for (_, name) in objects {
        *counts.entry(name.as_str()).or_insert(0) += 1;
    }
    objects.iter()
        .filter(|(_, name)| counts.get(name.as_str()) == Some(&1))
        .filter_map(|(id, name)| {
            Some((*id, boxes.get(name.as_str()).copied().flatten()?))
        })
        .collect()
}

pub fn read_3mf(data: Vec<u8>, reported_plate: Option<u32>)
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
    match &slice_info {
        Some(xml) => {
            if let Some(block) = plate_block(xml, plate) {
                let (objects, skipped, label) = parse_slice_info(block);
                bundle.objects = objects;
                bundle.skipped = skipped;
                bundle.label_objects = label;
            }
        }
        None => {
            if let Some(plate) = plate
                && let Some(xml) =
                    read_entry(&mut zip, "Metadata/model_settings.config")
            {
                bundle.objects = parse_model_settings(
                    &String::from_utf8_lossy(&xml), plate);
            }
        }
    }
    if let Some(plate) = plate
        && !bundle.objects.is_empty()
    {
        let ids: HashSet<i64> =
            bundle.objects.iter().map(|(id, _)| *id).collect();
        if let Some(png) =
            read_entry(&mut zip, &format!("Metadata/pick_{plate}.png"))
        {
            bundle.bboxes = pick_bboxes(&png, &ids);
        }
        if bundle.bboxes.is_empty()
            && let Some(raw) =
                read_entry(&mut zip, &format!("Metadata/plate_{plate}.json"))
            && let Ok(plate_json) =
                serde_json::from_slice::<serde_json::Value>(&raw)
        {
            bundle.bboxes = bboxes_by_name(&plate_json, &bundle.objects);
        }
    }
    label_copies(&mut bundle.objects);
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
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
        let flagged: Vec<(u32, i64, &str, bool)> = objects.iter()
            .map(|(index, iid, name)| (*index, *iid, *name, false))
            .collect();
        slice_info_with(&flagged, false)
    }

    /// The same, with each object's `skipped` flag and the
    /// `label_object_enabled` setting Studio writes into the plate block of
    /// a job whose objects can be skipped.
    fn slice_info_with(objects: &[(u32, i64, &str, bool)], label: bool)
                       -> String {
        let mut xml = String::from("<config>");
        let mut open = None;
        for (index, iid, name, skipped) in objects {
            if open != Some(*index) {
                if open.is_some() {
                    xml += "</plate>\n";
                }
                xml += &format!("<plate>\n  \
                    <metadata key=\"index\" value=\"{index}\"/>\n");
                if label {
                    xml += "  <metadata key=\"label_object_enabled\" \
                            value=\"true\"/>\n";
                }
                open = Some(*index);
            }
            xml += &format!("  <object identify_id=\"{iid}\" \
                name=\"{name}\" skipped=\"{skipped}\" />\n");
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
        pick_typed(candidates, job, file, "local")
    }

    fn pick_typed(candidates: &[&str], job: &str, file: &str,
                  print_type: &str) -> Option<String> {
        let candidates: Vec<String> =
            candidates.iter().map(|s| s.to_string()).collect();
        super::pick_3mf(&candidates, job, file, print_type)
    }

    /// A pick image; `paint` gives each pixel's (identify_id, alpha).
    fn pick_png(w: u32, h: u32, paint: impl Fn(u32, u32) -> (i64, u8))
                -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| {
            let (id, a) = paint(x, y);
            image::Rgba([id as u8, (id >> 8) as u8, (id >> 16) as u8, a])
        });
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
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

    /// The skip dialog starts from what the job says: the objects
    /// slice_info already marked skipped, and whether the job can skip at
    /// all (`label_object_enabled`, which the slicer writes only when the
    /// gcode labels its objects).
    #[test]
    fn skipped_objects_and_the_label_setting_come_from_slice_info() {
        let info = slice_info_with(&[(1, 11, "part", false),
                                     (1, 12, "lid", true),
                                     (1, 13, "base", false)], true);
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.objects.len(), 3);
        assert_eq!(bundle.skipped, HashSet::from([12]),
                   "only the object the job marked skipped");
        assert!(bundle.label_objects, "this job can skip objects");

        // without the setting nothing may be skipped, whatever the dialog
        // is asked to show
        let info = slice_info_with(&[(1, 11, "part", false),
                                     (1, 12, "lid", true)], false);
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.skipped, HashSet::from([12]));
        assert!(!bundle.label_objects,
                "a job without label_object_enabled cannot skip objects");
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
    fn local_jobs_prefer_the_root_upload() {
        // non-cloud print types look in the root first: LAN uploads land
        // there, and on the owner's cards the root copy was the newer one
        let found = pick(&["/cache/(Unsaved).3mf", "/(Unsaved).gcode.3mf"],
                         "(Unsaved)", "");
        assert_eq!(found.as_deref(), Some("/(Unsaved).gcode.3mf"));
        let found = pick(&["/cache/part.3mf", "/part.3mf"], "x", "part.3mf");
        assert_eq!(found.as_deref(), Some("/part.3mf"));
    }

    #[test]
    fn gcode_file_name_is_not_rewritten_to_a_cache_3mf() {
        // "X.gcode.3mf" used to be looked up as "X.3mf" too, which on the
        // cards only ever found an older cloud job of the same name
        assert_eq!(pick(&["/cache/part.3mf"], "", "part.gcode.3mf"), None);
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

    #[test]
    fn cloud_jobs_prefer_the_cache_copy() {
        let both = ["/cache/(Unsaved).3mf", "/(Unsaved).gcode.3mf"];
        assert_eq!(pick_typed(&both, "(Unsaved)", "", "cloud").as_deref(),
                   Some("/cache/(Unsaved).3mf"));
        assert_eq!(pick_typed(&both, "(Unsaved)", "", "local").as_deref(),
                   Some("/(Unsaved).gcode.3mf"));
        // X1 cloud prints report a ramdisk path and keep the subtask name
        let found = pick_typed(&["/Lovers.gcode.3mf", "/cache/Lovers.3mf"],
                               "Lovers", "/data/metadata/plate_3.gcode",
                               "cloud");
        assert_eq!(found.as_deref(), Some("/cache/Lovers.3mf"));
    }

    #[test]
    fn ramdisk_gcode_path_is_not_a_job_name() {
        let path = "/data/Metadata/plate_1.gcode";
        assert_eq!(pick(&["/Plate_1.gcode.3mf"], path, path), None);
    }

    #[test]
    fn studio_sanitised_upload_name_matches() {
        let file = "/cache/Card_Shuffler_V2_-_No_Screw,_No_Glue.3mf";
        let found = pick_typed(&[file], "Card Shuffler V2 - No Screw, No Glue",
                               "", "cloud");
        assert_eq!(found.as_deref(), Some(file));
        // the exact name still beats a sanitised look-alike
        let found = pick(&["/a_b.gcode.3mf", "/a b.gcode.3mf"], "a b", "");
        assert_eq!(found.as_deref(), Some("/a b.gcode.3mf"));
    }

    #[test]
    fn only_real_shortened_names_count() {
        // 101 characters ending in "..." is not a shortened name
        let stem = format!("{}...", "a".repeat(98));
        let file = format!("/{stem}.gcode.3mf");
        let job = format!("{}bcd", "a".repeat(98));
        assert_eq!(pick(&[file.as_str()], &job, ""), None);
        // 100 characters without "..." is a whole name
        let file = format!("/{}.gcode.3mf", "b".repeat(100));
        assert_eq!(pick(&[file.as_str()], &"b".repeat(103), ""), None);
        // the kept part has to match up to its last character
        let job = "c".repeat(120);
        let mut other = job.clone();
        other.replace_range(96..97, "x");
        let file = format!("/{}.gcode.3mf", shortened(&other));
        assert_eq!(pick(&[file.as_str()], &job, ""), None);
        // a short name ending in "..." is not shortened
        assert_eq!(pick(&["/cache/Wait for it....3mf"], "wait for it... v2",
                        ""), None);
    }

    #[test]
    fn shortened_non_ascii_names_match() {
        let job = "Soporte año ".repeat(10);
        let file = format!("/{}.gcode.3mf", shortened(&job));
        assert_eq!(pick(&[file.as_str()], &job, "").as_deref(),
                   Some(file.as_str()));
        // 97 bytes + "..." for a name counted in bytes
        let kept = format!("a{}", "爪".repeat(32));
        let file = format!("/cache/{kept}{}.3mf", "...");
        let job = format!("a{}", "爪".repeat(40));
        assert_eq!(pick(&[file.as_str()], &job, "").as_deref(),
                   Some(file.as_str()));
    }

    #[test]
    fn job_plate_needs_a_plate_suffix() {
        assert_eq!(job_plate("Nameplate_3", ""), None);
        assert_eq!(job_plate("X_plate_2_v3", ""), None);
        assert_eq!(job_plate("X_Plate_2 ", ""), Some(2));
    }

    #[test]
    fn slice_info_beats_a_different_reported_plate_without_gcode() {
        let plate_2 = slice_info(&[(2, 1, "a")]);
        assert_eq!(sliced_plate(&[], Some(&plate_2), Some(3)), Some(2));
    }

    #[test]
    fn plate_blocks_are_matched_by_index_number() {
        let info = slice_info(&[(10, 100, "ten"), (1, 1, "one")]);
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.objects, vec![(1, "one".to_string())]);
        // a lone block for another plate is not this plate's
        let info = slice_info(&[(1, 1, "one")]);
        let data = make_3mf(&[
            ("Metadata/plate_2.gcode", b"; gcode"),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        assert!(read_3mf(data, None).unwrap().objects.is_empty());
    }

    #[test]
    fn escaped_names_are_decoded_and_paired() {
        let info = slice_info(&[(1, 7, "Tom &amp; Jerry&apos;s.stl"),
                                (1, 8, "plain.stl")]);
        let json = plate_json(&[(20, "Tom & Jerry's.stl"),
                                (21, "plain.stl")]);
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_1.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.objects[0].1, "Tom & Jerry's.stl");
        assert!(bundle.bboxes.contains_key(&7));
        assert!(bundle.bboxes.contains_key(&8));
    }

    #[test]
    fn xml_unescape_decodes_once() {
        assert_eq!(super::xml_unescape("a &amp;lt; b"), "a &lt; b");
        assert_eq!(super::xml_unescape("&#65;&#x42;&quot;"), "AB\"");
        assert_eq!(super::xml_unescape("R&D &bogus; &"), "R&D &bogus; &");
    }

    #[test]
    fn copies_are_boxed_from_the_pick_image() {
        let info = slice_info(&[(1, 11, "part"), (1, 12, "part")]);
        let json = plate_json(&[(20, "part"), (21, "part")]);
        // 8x8 bed: copy 11 top-left, copy 12 bottom-middle; an unknown
        // colour and a half-transparent pixel are ignored
        let pick = pick_png(8, 8, |x, y| match (x, y) {
            (0..=1, 0..=1) => (11, 255),
            (4..=5, 6..=7) => (12, 255),
            (7, 0) => (99, 255),
            (7, 7) => (11, 128),
            _ => (0, 0),
        });
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/pick_1.png", pick.as_slice()),
            ("Metadata/plate_1.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.objects, vec![(11, "part".to_string()),
                                        (12, "part #2".to_string())]);
        assert_eq!(bundle.bboxes.get(&11), Some(&[0.0, 192.0, 64.0, 256.0]));
        assert_eq!(bundle.bboxes.get(&12), Some(&[128.0, 0.0, 192.0, 64.0]));
        assert_eq!(bundle.bboxes.len(), 2);
    }

    #[test]
    fn pick_image_without_known_ids_falls_back_to_names() {
        let info = slice_info(&[(1, 11, "lid")]);
        let json = plate_json(&[(20, "lid")]);
        let pick = pick_png(4, 4, |_, _| (99, 255));
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/pick_1.png", pick.as_slice()),
            ("Metadata/plate_1.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        assert!(read_3mf(data, None).unwrap().bboxes.contains_key(&11));
    }

    #[test]
    fn name_pairing_uses_names_not_order() {
        let info = slice_info(&[(1, 11, "lid"), (1, 12, "base")]);
        let json = r#"{"bbox_objects":[
            {"id":20,"name":"base","bbox":[1,1,2,2]},
            {"id":21,"name":"lid","bbox":[5,5,6,6]}]}"#;
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_1.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.bboxes.get(&11), Some(&[5.0, 5.0, 6.0, 6.0]));
        assert_eq!(bundle.bboxes.get(&12), Some(&[1.0, 1.0, 2.0, 2.0]));
    }

    #[test]
    // the fixture's type is written out on purpose: clearer here than an
    // alias used once
    #[allow(clippy::type_complexity)]
    fn one_sided_duplicates_and_bad_boxes_get_no_box() {
        let cases: [(&[(u32, i64, &str)], &str); 3] = [
            (&[(1, 11, "part"), (1, 12, "part")],
             r#"{"bbox_objects":[{"id":20,"name":"part","bbox":[1,1,2,2]}]}"#),
            (&[(1, 11, "part")],
             r#"{"bbox_objects":[{"id":20,"name":"part","bbox":[1,1,2,2]},
                {"id":21,"name":"part","bbox":[3,3,4,4]}]}"#),
            (&[(1, 11, "part")],
             r#"{"bbox_objects":[
                {"id":20,"name":"part","bbox":[1,2,3,4,5]}]}"#),
        ];
        for (objects, json) in cases {
            let info = slice_info(objects);
            let data = make_3mf(&[
                ("Metadata/plate_1.gcode", b"; gcode"),
                ("Metadata/plate_1.json", json.as_bytes()),
                ("Metadata/slice_info.config", info.as_bytes()),
            ]);
            assert!(read_3mf(data, None).unwrap().bboxes.is_empty(), "{json}");
        }
    }

    #[test]
    fn model_settings_fallback_only_for_the_chosen_plate() {
        let settings = "<config>\
            <object id=\"1\"><metadata key=\"name\" value=\"Cube\"/></object>\
            <object id=\"2\"><metadata key=\"name\" value=\"Lid\"/></object>\
            <plate><metadata key=\"plater_id\" value=\"1\"/>\
            <model_instance><metadata key=\"object_id\" value=\"1\"/>\
            <metadata key=\"identify_id\" value=\"10\"/></model_instance>\
            </plate>\
            <plate><metadata key=\"plater_id\" value=\"3\"/>\
            <model_instance><metadata key=\"object_id\" value=\"2\"/>\
            <metadata key=\"identify_id\" value=\"30\"/></model_instance>\
            </plate></config>";
        let data = make_3mf(&[
            ("Metadata/plate_3.gcode", b"; gcode"),
            ("Metadata/model_settings.config", settings.as_bytes()),
        ]);
        assert_eq!(read_3mf(data, None).unwrap().objects,
                   vec![(30, "Lid".to_string())]);
        // plate unknown: nothing, not every plate's objects
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_3.gcode", b"; gcode"),
            ("Metadata/model_settings.config", settings.as_bytes()),
        ]);
        assert!(read_3mf(data, None).unwrap().objects.is_empty());
        // with slice_info present its plate block alone decides
        let info = slice_info(&[(1, 10, "Cube")]);
        let data = make_3mf(&[
            ("Metadata/plate_3.gcode", b"; gcode"),
            ("Metadata/model_settings.config", settings.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        assert!(read_3mf(data, None).unwrap().objects.is_empty());
    }

    #[test]
    fn card_and_maker_forms_match_one_way_only() {
        // '|' is stored as '_' with the spaces kept
        let file = concat!("/cache/FAN GRILL V2 _ 0.2mm layer _ ",
                           "4 walls _ 17% infill.3mf");
        let found = pick_typed(&[file],
            "FAN GRILL V2 | 0.2mm layer | 4 walls | 17% infill", "", "cloud");
        assert_eq!(found.as_deref(), Some(file));
        // a MakerWorld title ending in a space keeps a trailing '_'
        let file = "/Playing_Cards_-_Minimal_.gcode.3mf";
        assert_eq!(pick(&[file], "Playing Cards - Minimal ", "").as_deref(),
                   Some(file));
        // Studio never turns '_' into a space
        assert_eq!(pick(&["/Part A.gcode.3mf"], "Part_A", ""), None);
        // symbols alone or doubled '_' are no Studio output
        assert_eq!(pick(&["/cache/_.3mf"], "?", ""), None);
        assert_eq!(pick(&["/cache/a__b.3mf"], "a b", ""), None);
    }

    #[test]
    fn shortened_card_form_of_a_long_title_matches() {
        let job = "Big Fan Grill | 0.2mm layer | 4 walls | 17% infill | \
                   lid, base, clips and a spare set of feet for the stand";
        let file = format!("/cache/{}.3mf", shortened(&super::card_form(job)));
        assert_eq!(pick_typed(&[file.as_str()], job, "", "cloud").as_deref(),
                   Some(file.as_str()));
    }

    #[test]
    fn copy_labels_never_repeat() {
        let info = slice_info(&[(1, 11, "part"), (1, 12, "part"),
                                (1, 13, "part #2")]);
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let labels: Vec<String> = read_3mf(data, None).unwrap().objects
            .into_iter().map(|(_, label)| label).collect();
        assert_eq!(labels, ["part", "part #3", "part #2"]);
    }

    #[test]
    fn numeric_references_must_be_plain_digits() {
        assert_eq!(super::xml_unescape("a&#+65;b"), "a&#+65;b");
        assert_eq!(super::xml_unescape("&#x+41;"), "&#x+41;");
        assert_eq!(super::xml_unescape("&#0;"), "&#0;");
    }
}
