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
