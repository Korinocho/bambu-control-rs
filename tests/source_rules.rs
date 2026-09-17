//! Source scans that guard the TLS design (design doc 5.3). They live in
//! tests/, outside the scanned src tree, so their own patterns never match.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("readable source dir");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// (file, line number, line) for every line containing any pattern.
fn hits(files: &[PathBuf], patterns: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(file).expect("utf-8 source");
        for (i, line) in text.lines().enumerate() {
            if patterns.iter().any(|p| line.contains(p)) {
                found.push(format!("{}:{}: {}", file.display(), i + 1,
                                   line.trim()));
            }
        }
    }
    found
}

/// The vendored suppaftp's sources (vendor/suppaftp/PATCHES.md).
fn vendored_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    rust_files(&root().join("vendor").join("suppaftp").join("src"), &mut files);
    assert!(files.len() > 10, "scanned {} vendored files", files.len());
    files
}

/// T23: the CI grep pattern, as plain substrings, over src and the vendored
/// suppaftp.
#[test]
fn no_provider_default_calls_in_src() {
    let mut files = Vec::new();
    rust_files(&root().join("src"), &mut files);
    assert!(files.len() > 5, "scanned {} files", files.len());
    files.extend(vendored_files());
    let found = hits(&files, &[
        "install_default",
        "get_default(",
        "ClientConfig::builder()",
        "ServerConfig::builder()",
        "builder_with_protocol_versions(",
    ]);
    assert!(found.is_empty(), "provider rule violated:\n{}",
            found.join("\n"));
}

/// T13: one verification path, over all of src (the tls module and every
/// FTPS caller). clippy.toml bans the same helpers, alias forms included.
/// The bare word webpki is allowed, because the required comment in
/// verify_tls12_signature contains it.
#[test]
fn tls_module_never_calls_the_rustls_signature_helper() {
    let mut files = Vec::new();
    rust_files(&root().join("src"), &mut files);
    assert!(files.iter().any(|f| f.ends_with("tls.rs"))
                && files.iter().any(|f| f.ends_with("connector.rs")),
            "scanned {} files", files.len());
    let found = hits(&files, &[
        "crypto::verify_tls12_signature(",
        "crypto::verify_tls13_signature(",
        "webpki::",
        "rustls_webpki",
        "WebPkiServerVerifier",
    ]);
    assert!(found.is_empty(), "second verification path:\n{}",
            found.join("\n"));
}

/// Design doc 4, rule 6: `main` takes the single-instance mutex before it
/// opens a window, and a second launch says so and exits without touching a
/// printer. The mutex itself has unit tests; `main()` has none, so its
/// wiring is scanned here.
#[test]
fn main_takes_the_single_instance_mutex() {
    let main = root().join("src").join("main.rs");
    let text = std::fs::read_to_string(&main).expect("utf-8 source");
    for needle in ["instance::acquire(instance::MUTEX_NAME)",
                   "instance::show_already_running()",
                   "Err(instance::AlreadyRunning)"] {
        assert!(text.contains(needle),
                "src/main.rs no longer has {needle} (design doc 4, rule 6)");
    }
}

/// The `port` parameter of `Session::open` exists so the tests can reach an
/// in-process broker. That makes it the one argument no test can check: to
/// reach a broker a test must pass a different port by construction, so the
/// parity test asserts the timing, the credentials and the client id, and
/// structurally cannot assert this. It is asserted from the source instead.
///
/// Without this, the port parameter is the single way production could talk
/// to the wrong port with every test still green.
#[test]
fn production_mqtt_sites_pass_the_port_constant() {
    let text = std::fs::read_to_string(root().join("src").join("mqtt.rs"))
        .expect("utf-8 source");
    // the test module has call sites of its own, and those are supposed to
    // pass a broker's port; only the callers above it are production
    let production = text.split("\nmod tests {").next().unwrap_or(&text);
    // every call that names a port must name the constant: Session::open in
    // PrinterClient::run and in the probe, and the public probe delegating
    // to its port-taking form
    for (at, _) in production.match_indices("Session::open(") {
        let call = &production[at..(at + 240).min(production.len())];
        assert!(call.contains("MQTT_PORT") || call.contains(", port,"),
                "a Session::open call site passes neither MQTT_PORT nor a \
                 port threaded from one:\n{call}");
    }
    // the entry point must hand its inner form BOTH the anchor built from
    // the configured serial and the real port: those are the two arguments
    // the parity test cannot check, because it must pass others to run
    assert!(production.contains(
                "subscribe_gcode_state_at(&tls, ip, MQTT_PORT, serial,"),
            "the public probe must pass PrinterTls::new(serial)'s anchor and \
             MQTT_PORT to its inner form");
    assert!(production.contains("let tls = match PrinterTls::new(serial)"),
            "the public probe must build its anchor from the configured \
             serial, not take one");
    // prose may name the port; code may not. A literal at a call site is
    // the failure this counts, and a doc comment is not one.
    let literals = production.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| line.contains("8883"))
        .count();
    assert_eq!(literals, 1,
               "8883 belongs only in the MQTT_PORT constant, found \
                {literals} outside comments");
}

/// The literals docs/gui-polish-guidelines.md 1.11 keeps out of the UI code:
/// every colour, font size, radius, margin, spacing, stroke and widget size
/// is a `theme` token (T1, T2).
const TOKEN_RULES: &[(&str, &str)] = &[
    ("colour", r"Color32::from_(rgb|rgba_unmultiplied|rgba_premultiplied|gray|black_alpha|white_alpha)\("),
    ("colour", r"Color32::(BLACK|WHITE)\b"),
    ("font", r"\.size\(\s*[0-9]"),
    ("font", r"FontId::(new|proportional|monospace)\("),
    ("font", r"bold\(\s*[0-9]"),
    ("radius", r"CornerRadius::same\(\s*[0-9]"),
    ("radius", r"corner_radius\(\s*[0-9]"),
    ("margin", r"Margin::(same|symmetric)\(\s*[0-9]"),
    ("margin", r"inner_margin\(\s*[0-9]"),
    ("spacing", r"add_space\(\s*[0-9]"),
    ("spacing", r"item_spacing(\.[xy])?\s*="),
    ("stroke", r"Stroke::new\(\s*[0-9]"),
    ("size", r"vec2\(\s*[0-9.]+\s*,\s*[0-9]"),
    ("size", r"desired_(width|height)\(\s*[0-9]"),
    ("size", r"\.width\(\s*[0-9]"),
    ("size", r"max_height\(\s*[0-9]"),
];

/// `src/ui/*.rs` and `src/main.rs`, each up to its unit-test module. The cut
/// is `#[cfg(test)]` followed by `mod tests`, not the first `#[cfg(test)]`:
/// main.rs declares the test-only `snapshots` module near its top.
fn ui_production_sources() -> Vec<(PathBuf, String)> {
    let ui = root().join("src").join("ui");
    let mut files: Vec<PathBuf> = std::fs::read_dir(ui)
        .expect("readable src/ui")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.push(root().join("src").join("main.rs"));
    files.sort();
    files.into_iter()
        .map(|file| {
            let text = std::fs::read_to_string(&file).expect("utf-8 source");
            let production = text.split("#[cfg(test)]\nmod tests")
                .next().unwrap_or(&text).to_string();
            (file, production)
        })
        .collect()
}

/// Every line matching a rule, skipping named consts and comments (1.11).
fn token_hits(file: &Path, text: &str) -> Vec<String> {
    let rules: Vec<(&str, regex_lite::Regex)> = TOKEN_RULES.iter()
        .map(|(kind, pattern)| (*kind, regex_lite::Regex::new(pattern)
            .expect("a valid pattern")))
        .collect();
    let mut found = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("const ") || trimmed.starts_with("pub const ")
            || trimmed.starts_with("//")
        {
            continue;
        }
        for (kind, rule) in &rules {
            if rule.is_match(line) {
                found.push(format!("{}:{}: {kind}: {}", file.display(), i + 1,
                                   line.trim()));
            }
        }
    }
    found
}

/// T2: the UI code takes every visual value from `theme` (1.11).
#[test]
fn ui_code_uses_theme_tokens_only() {
    let sources = ui_production_sources();
    // the scan read what it claims to: every UI file, with its code in it
    for needle in ["main.rs", "panel.rs", "files_view.rs", "dialogs.rs",
                   "widgets.rs"] {
        let (_, text) = sources.iter()
            .find(|(file, _)| file.ends_with(needle))
            .unwrap_or_else(|| panic!("{needle} was not scanned"));
        assert!(text.lines().count() > 100,
                "{needle}: only {} lines scanned", text.lines().count());
    }
    let main = &sources.iter().find(|(f, _)| f.ends_with("main.rs"))
        .expect("main.rs").1;
    assert!(main.contains("fn chips_bar("), "main.rs was cut short");
    let found: Vec<String> = sources.iter()
        .flat_map(|(file, text)| token_hits(file, text))
        .collect();
    assert!(found.is_empty(), "literals outside theme.rs:\n{}",
            found.join("\n"));
}

/// The control for the scan above: each rule finds the literal it names, and
/// a named const or a comment is left alone.
#[test]
fn the_token_scan_finds_each_kind_of_literal() {
    let sample = "\
        let a = Color32::from_rgb(1, 2, 3);\n\
        let b = Color32::WHITE;\n\
        text.size(11.0);\n\
        FontId::proportional(13.0);\n\
        theme::bold(12.0);\n\
        CornerRadius::same(10);\n\
        frame.corner_radius(12);\n\
        egui::Margin::symmetric(10, 4);\n\
        frame.inner_margin(8);\n\
        ui.add_space(6.0);\n\
        ui.spacing_mut().item_spacing.y = 3.0;\n\
        Stroke::new(1.0, theme::BORDER);\n\
        egui::vec2(36.0, 30.0);\n\
        edit.desired_width(180.0);\n\
        combo.width(72.0);\n\
        area.max_height(150.0);\n\
        const NAMED: Vec2 = vec2(16.0, 12.0);\n\
        // add_space(4.0) in prose\n\
        ui.add_space(space::M);\n";
    let found = token_hits(Path::new("sample.rs"), sample);
    let lines: Vec<usize> = found.iter()
        .map(|hit| hit.split(':').nth(1).expect("a line number").parse()
            .expect("a number"))
        .collect();
    assert_eq!(lines, (1..=16).collect::<Vec<_>>(), "{found:#?}");
}

/// Every `ScrollArea` statement names its state with `id_salt` before it is
/// shown (E1), and no `push_id` is keyed by a row index (E2). Returns what
/// breaks either rule.
fn identity_hits(file: &Path, text: &str) -> Vec<String> {
    let scroll = regex_lite::Regex::new(
        r"ScrollArea::(vertical|horizontal|both)\(\)").expect("pattern");
    let show = regex_lite::Regex::new(r"\.show(_viewport|_rows)?\(")
        .expect("pattern");
    let by_index = regex_lite::Regex::new(r"push_id\(\s*(row|index|i)\s*,")
        .expect("pattern");
    let line_of = |at: usize| text[..at].lines().count();
    let mut found = Vec::new();
    for start in scroll.find_iter(text) {
        let statement = match show.find(&text[start.end()..]) {
            Some(end) => &text[start.end()..start.end() + end.start()],
            None => &text[start.end()..],
        };
        if !statement.contains(".id_salt(") {
            found.push(format!("{}:{}: ScrollArea without id_salt",
                               file.display(), line_of(start.start())));
        }
    }
    for hit in by_index.find_iter(text) {
        found.push(format!("{}:{}: push_id keyed by an index",
                           file.display(), line_of(hit.start())));
    }
    found
}

/// E1, E2: scroll state and row ids have names of their own.
#[test]
fn ui_state_is_keyed_by_identity() {
    let sources = ui_production_sources();
    let scrolls = sources.iter()
        .map(|(_, text)| text.matches("ScrollArea::vertical()").count())
        .sum::<usize>();
    assert!(scrolls >= 8, "only {scrolls} scroll areas scanned");
    let found: Vec<String> = sources.iter()
        .flat_map(|(file, text)| identity_hits(file, text))
        .collect();
    assert!(found.is_empty(), "unnamed state:\n{}", found.join("\n"));
}

/// The control for the scan above.
#[test]
fn the_identity_scan_finds_unnamed_state() {
    let sample = "\
        egui::ScrollArea::vertical().max_height(9.0).show(ui, |ui| {});\n\
        egui::ScrollArea::vertical().id_salt(\"x\").show(ui, |ui| {});\n\
        ui.push_id(row, |ui| {});\n\
        ui.push_id(transfer.id, |ui| {});\n";
    let found = identity_hits(Path::new("sample.rs"), sample);
    assert_eq!(found, ["sample.rs:1: ScrollArea without id_salt",
                       "sample.rs:3: push_id keyed by an index"]);
}

/// T24 (the CI grep, locally): no certificate checks disabled anywhere.
///
/// Every path to a printer verifies now -- FTPS on 990, the camera on 6000
/// (issue #2) and MQTT on 8883 (issue #1) -- so this scans all of `src`
/// plus the vendored suppaftp, exactly as `.github/workflows/ci.yml` does.
///
/// The two lists must stay equal. A local mirror that has drifted is worse
/// than no mirror at all: it passes for a file CI would fail on, so whoever
/// adds a danger flag there is told "green" locally and cannot see why the
/// build broke.
#[test]
fn ftps_code_never_disables_certificate_checks() {
    let src = root().join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    files.extend(vendored_files());
    let found = hits(&files, &["danger_accept_invalid_certs",
                               "danger_accept_invalid_hostnames"]);
    assert!(found.is_empty(), "certificate checks disabled:\n{}",
            found.join("\n"));
}
