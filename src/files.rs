//! Which 3mf on the SD card belongs to a printer job, and the bundle the
//! skip-objects dialog shows. Port of the Python `core/files.py`.
//!
//! The reading of a 3mf moved to src/threemf.rs (design doc 5.7) with every
//! phase 0 rule and its tests; what stays here is `JobBundle` and the
//! matcher, whose rules are section 10.1's. The FTPS session code lives in
//! src/ftp.rs and the worker that drives it in src/browser.rs, so nothing
//! here touches the network.

use std::collections::{HashMap, HashSet};

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

/// The matcher's rules (section 10.1), on names taken from the owner's
/// three cards. No printer file is a fixture.
#[cfg(test)]
mod tests {
    use super::{job_plate, pick_3mf};

    fn pick(candidates: &[&str], job: &str, file: &str) -> Option<String> {
        pick_typed(candidates, job, file, "local")
    }

    fn pick_typed(candidates: &[&str], job: &str, file: &str,
                  print_type: &str) -> Option<String> {
        let candidates: Vec<String> =
            candidates.iter().map(|s| s.to_string()).collect();
        pick_3mf(&candidates, job, file, print_type)
    }

    /// Studio's shortened upload name: the first 97 characters + "...".
    fn shortened(name: &str) -> String {
        let kept: String = name.chars().take(97).collect();
        format!("{kept}...")
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
    fn job_plate_needs_a_plate_suffix() {
        assert_eq!(job_plate("Nameplate_3", ""), None);
        assert_eq!(job_plate("X_plate_2_v3", ""), None);
        assert_eq!(job_plate("X_Plate_2 ", ""), Some(2));
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
}
