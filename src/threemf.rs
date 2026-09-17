//! Reading a `.gcode.3mf` (design doc 5.7). It holds the plate picture, the
//! sliced plate's objects and their boxes, and the metadata the detail pane
//! shows: time, weight, filaments, printer model and slicer warnings.
//!
//! The parsing moved here from `files.rs` with every phase 0 rule and test
//! (section 10.1): the plate comes from the file's single `plate_N.gcode`,
//! else the reported plate, else a lone `slice_info` index; object ids are
//! `slice_info` `identify_id`s, which are the ids the printer labels objects
//! with; boxes come from `pick_N.png`, with name pairing as the fallback.
//! `files.rs` keeps `JobBundle` and the matcher that picks which 3mf on the
//! card belongs to a job.
//!
//! A 3mf comes off the SD card, so nothing here trusts its contents (5.1,
//! rule 6): entries are read through `ZipArchive` (the files use data
//! descriptors, so the streaming reader fails on them), every number is a
//! checked parse, and the G-code entry is read up to a byte cap instead of
//! whole.

// `inspect` and `plate_gcode` are the detail pane's, which is stage 3,
// part 2; `read_3mf` has callers today. Test builds are not excused.
#![cfg_attr(not(test), allow(dead_code,
    reason = "the detail pane (stage 3, part 2) is the first caller"))]

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};

use regex_lite::Regex;
use serde::{Deserialize, Serialize};

use crate::files::JobBundle;
use crate::gcode::{self, HEADER_READ_MAX};

/// Largest `plate_N.gcode` this module inflates into memory. The `/cache`
/// copies on the owner's cards reach ~106 MB, and the copy inside a 3mf is
/// 5-6x smaller (3.3), so this clears any real file while keeping a corrupt
/// or hostile archive from deciding how much memory the process takes.
pub const PLATE_GCODE_MAX: u64 = 64 * 1024 * 1024;

type Zip = zip::ZipArchive<Cursor<Vec<u8>>>;

/// One filament of the sliced plate, as `slice_info` reports it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Filament {
    /// `type`, which is a reserved word in Rust
    pub kind: String,
    pub color: String,
    pub used_g: Option<f32>,
    pub used_m: Option<f32>,
}

/// What a 3mf says about itself (design doc 5.7).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ThreeMfInfo {
    /// the sliced plate: a lone `plate_N.gcode`, the reported plate, or a
    /// lone `slice_info` index
    pub plate: Option<u32>,
    pub has_gcode: bool,
    /// `slice_info` `printer_model_id`: C12 = P1S, N2S = A1 ...
    pub printer_model_id: String,
    /// `project_settings` `printer_model`
    pub printer_model: String,
    pub prediction_s: Option<u32>,
    pub weight_g: Option<f32>,
    /// from the plate G-code's header block
    pub layers: Option<u32>,
    pub max_z_mm: Option<f32>,
    pub bed_type: String,
    pub slicer_version: String,
    pub filaments: Vec<Filament>,
    /// `slice_info` `identify_id` = the G-code's `OBJECT_ID`
    pub objects: Vec<(i64, String)>,
    /// id -> [x1, y1, x2, y2] on a 256-unit bed, y up
    pub bboxes: HashMap<i64, [f32; 4]>,
    /// slicer warnings, for example `not_support_traditional_timelapse`
    pub warnings: Vec<String>,
}

/// Largest entry inflated whole: the `.config` files and the plate and pick
/// pictures. The wire caps bound the *compressed* archive only, so a 64 MB
/// 3mf whose config entry is highly compressible would otherwise inflate
/// without limit, and an allocation failure aborts a release build (5.1,
/// rule 6). Studio's own entries are a few hundred KB at most.
const ENTRY_MAX: u64 = 8 * 1024 * 1024;

/// One entry, whole, up to `ENTRY_MAX`. An entry that reaches the cap is
/// refused rather than returned half-read: a truncated `slice_info` would
/// parse into a plausible, wrong object list.
fn read_entry(zip: &mut Zip, name: &str) -> Option<Vec<u8>> {
    let file = zip.by_name(name).ok()?;
    let mut buf = Vec::new();
    // one byte past the cap, so a read that fills it is known to be cut off
    file.take(ENTRY_MAX + 1).read_to_end(&mut buf).ok()?;
    (buf.len() as u64 <= ENTRY_MAX).then_some(buf)
}

/// The first `max` bytes of an entry, without inflating the rest: the plate
/// G-code is tens of megabytes and only its header block is wanted.
fn read_head(zip: &mut Zip, name: &str, max: u64) -> Option<Vec<u8>> {
    let file = zip.by_name(name).ok()?;
    let mut buf = Vec::new();
    file.take(max).read_to_end(&mut buf).ok()?;
    Some(buf)
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

/// What one plate contributes to a bundle: its objects, the ids the job
/// already marked skipped, whether anything may be skipped at all, and the
/// objects' boxes.
type PlateObjects = (Vec<(i64, String)>, HashSet<i64>, bool,
                     HashMap<i64, [f32; 4]>);

/// Objects and their boxes for one plate, shared by `read_3mf` and
/// `inspect`.
fn objects_and_boxes(zip: &mut Zip, slice_info: Option<&str>,
                     plate: Option<u32>) -> PlateObjects {
    let mut objects = Vec::new();
    let mut skipped = HashSet::new();
    let mut label = true;
    match slice_info {
        Some(xml) => {
            if let Some(block) = plate_block(xml, plate) {
                let (found, marked, enabled) = parse_slice_info(block);
                objects = found;
                skipped = marked;
                label = enabled;
            }
        }
        None => {
            if let Some(plate) = plate
                && let Some(xml) =
                    read_entry(zip, "Metadata/model_settings.config")
            {
                objects = parse_model_settings(
                    &String::from_utf8_lossy(&xml), plate);
            }
        }
    }
    let mut bboxes = HashMap::new();
    if let Some(plate) = plate
        && !objects.is_empty()
    {
        let ids: HashSet<i64> = objects.iter().map(|(id, _)| *id).collect();
        if let Some(png) =
            read_entry(zip, &format!("Metadata/pick_{plate}.png"))
        {
            bboxes = pick_bboxes(&png, &ids);
        }
        if bboxes.is_empty()
            && let Some(raw) =
                read_entry(zip, &format!("Metadata/plate_{plate}.json"))
            && let Ok(plate_json) =
                serde_json::from_slice::<serde_json::Value>(&raw)
        {
            bboxes = bboxes_by_name(&plate_json, &objects);
        }
    }
    label_copies(&mut objects);
    (objects, skipped, label, bboxes)
}

/// The sliced plate's picture, in Studio's order of preference.
fn plate_png(zip: &mut Zip, plate: Option<u32>) -> Option<Vec<u8>> {
    let plate = plate?;
    // this plate's images only — another plate's picture would mislead
    [format!("Metadata/plate_{plate}.png"),
     format!("Metadata/top_{plate}.png")].iter()
        .find_map(|name| read_entry(zip, name))
}

/// The plate picture and skippable objects of a job's 3mf, for the
/// skip-objects dialog. Phase 0's rules, unchanged (section 10.1).
pub fn read_3mf(data: Vec<u8>, reported_plate: Option<u32>)
                -> anyhow::Result<JobBundle> {
    let mut zip = zip::ZipArchive::new(Cursor::new(data))?;
    let names: Vec<String> = zip.file_names().map(str::to_string).collect();
    let slice_info = read_entry(&mut zip, "Metadata/slice_info.config")
        .map(|xml| String::from_utf8_lossy(&xml).to_string());
    let plate = sliced_plate(&names, slice_info.as_deref(), reported_plate);
    let (objects, skipped, label, bboxes) =
        objects_and_boxes(&mut zip, slice_info.as_deref(), plate);
    let mut bundle = JobBundle {
        plate_png: plate_png(&mut zip, plate),
        objects,
        bboxes,
        skipped,
        ..Default::default()
    };
    // assigned rather than initialised, as phase 0 set it: this is the
    // flag the skip dialog reads to decide whether anything may be skipped
    bundle.label_objects = label;
    Ok(bundle)
}

/// `<metadata key="k" value="v"/>` pairs of one block.
fn metadata_of(block: &str) -> HashMap<String, String> {
    let re = Regex::new(r#"<metadata key="([^"]*)" value="([^"]*)""#).unwrap();
    re.captures_iter(block)
        .map(|c| (c.get(1).unwrap().as_str().to_string(),
                  xml_unescape(c.get(2).unwrap().as_str())))
        .collect()
}

/// `<filament .../>` entries of one plate block.
fn filaments_of(block: &str) -> Vec<Filament> {
    let re_filament = Regex::new(r"<filament\s+([^>]*?)/>").unwrap();
    let re_attr = Regex::new(r#"(\w+)="([^"]*)""#).unwrap();
    re_filament.captures_iter(block)
        .map(|cap| {
            let attrs: HashMap<&str, &str> = re_attr
                .captures_iter(cap.get(1).unwrap().as_str())
                .map(|c| (c.get(1).unwrap().as_str(),
                          c.get(2).unwrap().as_str()))
                .collect();
            let text = |key| attrs.get(key).map(|s| xml_unescape(s))
                .unwrap_or_default();
            let number = |key| attrs.get(key)
                .and_then(|s: &&str| s.parse::<f32>().ok());
            Filament {
                kind: text("type"),
                color: text("color"),
                used_g: number("used_g"),
                used_m: number("used_m"),
            }
        })
        .collect()
}

/// Slicer warnings of one plate block, for example the TPU jobs'
/// `not_support_traditional_timelapse` (design doc 5.5).
fn warnings_of(block: &str) -> Vec<String> {
    let re = Regex::new(r#"<warning[^>]*?msg="([^"]*)""#).unwrap();
    re.captures_iter(block)
        .map(|c| c.get(1).unwrap().as_str().to_string())
        .collect()
}

/// A `project_settings.config` value, which Studio writes as a string or as
/// an array of strings.
fn setting(json: &serde_json::Value, key: &str) -> String {
    match json.get(key) {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(items)) => items.first()
            .and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        _ => String::new(),
    }
}

/// What the detail pane shows about a 3mf, plus the sliced plate's picture
/// (design doc 5.7).
pub fn inspect(zip_bytes: &[u8])
               -> anyhow::Result<(ThreeMfInfo, Option<Vec<u8>>)> {
    let mut zip = zip::ZipArchive::new(Cursor::new(zip_bytes.to_vec()))?;
    let names: Vec<String> = zip.file_names().map(str::to_string).collect();
    let slice_info = read_entry(&mut zip, "Metadata/slice_info.config")
        .map(|xml| String::from_utf8_lossy(&xml).to_string());
    let plate = sliced_plate(&names, slice_info.as_deref(), None);
    let mut info = ThreeMfInfo { plate, ..Default::default() };

    if let Some(xml) = &slice_info {
        // the header carries the slicer version
        info.slicer_version = Regex::new(
            r#"<header_item key="X-BBL-Client-Version" value="([^"]*)""#)
            .unwrap()
            .captures(xml)
            .map(|c| c.get(1).unwrap().as_str().to_string())
            .unwrap_or_default();
        if let Some(block) = plate_block(xml, plate) {
            let meta = metadata_of(block);
            info.printer_model_id =
                meta.get("printer_model_id").cloned().unwrap_or_default();
            info.prediction_s =
                meta.get("prediction").and_then(|v| v.parse().ok());
            info.weight_g = meta.get("weight").and_then(|v| v.parse().ok());
            info.filaments = filaments_of(block);
            info.warnings = warnings_of(block);
        }
    }
    if let Some(raw) = read_entry(&mut zip, "Metadata/project_settings.config")
        && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&raw)
    {
        info.printer_model = setting(&json, "printer_model");
        info.bed_type = setting(&json, "curr_bed_type");
        if info.slicer_version.is_empty() {
            info.slicer_version = setting(&json, "version");
        }
    }
    if let Some(plate) = plate {
        let entry = format!("Metadata/plate_{plate}.gcode");
        info.has_gcode = names.contains(&entry);
        // only the header block: the whole entry is tens of megabytes
        if let Some(head) = read_head(&mut zip, &entry,
                                      HEADER_READ_MAX as u64)
        {
            let header = gcode::parse_header(&head);
            info.layers = header.layers;
            info.max_z_mm = header.max_z_mm;
            if info.prediction_s.is_none() {
                info.prediction_s = header.prediction_s;
            }
            if info.weight_g.is_none() {
                info.weight_g = header.weight_g;
            }
        }
    }
    let (objects, _, _, bboxes) =
        objects_and_boxes(&mut zip, slice_info.as_deref(), plate);
    info.objects = objects;
    info.bboxes = bboxes;
    let picture = plate_png(&mut zip, plate);
    Ok((info, picture))
}

/// The plate's G-code, for the layer preview of v2 (section 8). It is
/// bounded: a corrupt or hostile archive may not decide how much memory
/// this process takes (5.1, rule 6).
pub fn plate_gcode(zip_bytes: &[u8], plate: u32) -> anyhow::Result<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(Cursor::new(zip_bytes.to_vec()))?;
    let name = format!("Metadata/plate_{plate}.gcode");
    let entry = zip.by_name(&name)?;
    let size = entry.size();
    if size > PLATE_GCODE_MAX {
        anyhow::bail!("{name} is too big to read here ({size} B, limit \
                       {PLATE_GCODE_MAX} B)");
    }
    let mut data = Vec::new();
    // the declared size is the archive's word: read to the cap either way
    entry.take(PLATE_GCODE_MAX).read_to_end(&mut data)?;
    Ok(data)
}

/// The phase 0 rules (section 10.1) on synthetic zips, moved here with the
/// parsing, plus `inspect` and `plate_gcode`. No printer file is a fixture.
#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;

    use super::{ENTRY_MAX, ThreeMfInfo, inspect, plate_gcode, read_3mf,
                sliced_plate};

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
    fn numeric_references_must_be_plain_digits() {
        assert_eq!(super::xml_unescape("a&#+65;b"), "a&#+65;b");
        assert_eq!(super::xml_unescape("&#x+41;"), "&#x+41;");
        assert_eq!(super::xml_unescape("&#0;"), "&#0;");
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

    // ------------------------------------------------------------ inspect

    /// A job 3mf as Studio writes one, with the fields the detail pane
    /// shows (design doc 5.7).
    fn full_3mf() -> Vec<u8> {
        let info = "<config>\
            <header>\
            <header_item key=\"X-BBL-Client-Version\" value=\"01.10.01.50\"/>\
            </header>\
            <plate>\
            <metadata key=\"index\" value=\"1\"/>\
            <metadata key=\"printer_model_id\" value=\"C12\"/>\
            <metadata key=\"prediction\" value=\"578\"/>\
            <metadata key=\"weight\" value=\"0.26\"/>\
            <metadata key=\"label_object_enabled\" value=\"true\"/>\
            <object identify_id=\"484\" name=\"Soporte.stl\" \
                skipped=\"false\" />\
            <filament id=\"1\" type=\"PETG\" color=\"#161616\" \
                used_m=\"0.09\" used_g=\"0.26\"/>\
            <warning msg=\"not_support_traditional_timelapse\" level=\"1\"/>\
            </plate></config>";
        let settings = r#"{"printer_model":"Bambu Lab P1S",
                           "curr_bed_type":"Textured PEI Plate"}"#;
        let header = "; HEADER_BLOCK_START\n\
                      ; total layer number: 46\n\
                      ; max_z_height: 5.60\n\
                      ; HEADER_BLOCK_END\n";
        make_3mf(&[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/plate_1.gcode", header.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
            ("Metadata/project_settings.config", settings.as_bytes()),
        ])
    }

    #[test]
    fn inspect_reads_what_the_detail_pane_shows() {
        let (info, picture) = inspect(&full_3mf()).unwrap();
        assert_eq!(info.plate, Some(1));
        assert!(info.has_gcode);
        assert_eq!(info.printer_model_id, "C12");
        assert_eq!(info.printer_model, "Bambu Lab P1S");
        assert_eq!(info.bed_type, "Textured PEI Plate");
        assert_eq!(info.slicer_version, "01.10.01.50");
        assert_eq!(info.prediction_s, Some(578));
        assert_eq!(info.weight_g, Some(0.26));
        // layers and max Z come from the plate gcode's header block
        assert_eq!(info.layers, Some(46));
        assert_eq!(info.max_z_mm, Some(5.60));
        assert_eq!(info.objects, vec![(484, "Soporte.stl".to_string())]);
        assert_eq!(info.filaments.len(), 1);
        assert_eq!(info.filaments[0].kind, "PETG");
        assert_eq!(info.filaments[0].color, "#161616");
        assert_eq!(info.filaments[0].used_g, Some(0.26));
        // 5.5: the TPU warning is why a job has no timelapse
        assert_eq!(info.warnings, ["not_support_traditional_timelapse"]);
        assert_eq!(picture.as_deref(), Some(&b"plate-1"[..]));
    }

    /// A file that carries none of it reports nothing, and never guesses.
    #[test]
    fn inspect_of_a_bare_3mf_reports_nothing() {
        let data = make_3mf(&[("Metadata/plate_1.gcode", b"; gcode")]);
        let (info, picture) = inspect(&data).unwrap();
        assert_eq!(info, ThreeMfInfo {
            plate: Some(1), has_gcode: true, ..Default::default() });
        assert_eq!(picture, None);
        // and a file that is not a zip is an error, not a panic
        assert!(inspect(b"not a zip at all").is_err());
        assert!(inspect(&[]).is_err());
    }

    /// 5.1, rule 6: the plate gcode is read up to a cap, so a corrupt or
    /// hostile archive cannot decide how much memory this process takes.
    #[test]
    fn plate_gcode_is_read_and_bounded() {
        let body = vec![b'x'; 4096];
        let data = make_3mf(&[("Metadata/plate_2.gcode", body.as_slice())]);
        assert_eq!(plate_gcode(&data, 2).unwrap().len(), 4096);
        // a plate that is not in the file is an error
        assert!(plate_gcode(&data, 1).is_err());
        assert!(plate_gcode(b"not a zip", 1).is_err());
    }

    /// 5.1, rule 6: the entries read whole are bounded too. The wire caps
    /// bound the *compressed* archive, so a small 3mf whose config entry
    /// inflates to gigabytes would otherwise decide how much memory this
    /// process takes — and an allocation failure aborts a release build.
    #[test]
    fn an_entry_that_inflates_past_the_cap_is_refused() {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("Metadata/plate_1.gcode", opts).unwrap();
        zip.write_all(b"; gcode").unwrap();
        zip.start_file("Metadata/slice_info.config", opts).unwrap();
        // zeros: kilobytes on the wire, past the cap once inflated
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..=(ENTRY_MAX >> 20) {
            zip.write_all(&chunk).unwrap();
        }
        let data = zip.finish().unwrap().into_inner();
        assert!((data.len() as u64) < ENTRY_MAX / 8,
                "the fixture is not a compression bomb ({} B)", data.len());

        // it reads as an archive that says nothing, not as a crash and not
        // as a plausible object list parsed out of a half-read entry
        let (info, picture) = inspect(&data).unwrap();
        assert!(info.objects.is_empty(), "a cut entry was parsed anyway");
        assert_eq!(picture, None);
        assert!(read_3mf(data, None).unwrap().objects.is_empty());
    }
}
