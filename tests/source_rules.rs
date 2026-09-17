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

/// T24 (the CI grep, locally): no certificate checks disabled in the code
/// that verifies certificates -- FTPS, the camera since issue #2, and the
/// vendored suppaftp.
///
/// This list must stay equal to the one in `.github/workflows/ci.yml`. A
/// local mirror that has drifted is worse than no mirror at all: it passes
/// for a file CI would fail on, so whoever adds a danger flag there is told
/// "green" locally and cannot see why the build broke.
#[test]
fn ftps_code_never_disables_certificate_checks() {
    let src = root().join("src");
    let mut files = vec![src.join("tls.rs"), src.join("files.rs")];
    rust_files(&src.join("tls"), &mut files);
    files.extend(vendored_files());
    for later in ["ftp.rs", "browser.rs", "camera.rs"] {
        if src.join(later).exists() {
            files.push(src.join(later));
        }
    }
    let found = hits(&files, &["danger_accept_invalid_certs",
                               "danger_accept_invalid_hostnames"]);
    assert!(found.is_empty(), "danger flags in FTPS code:\n{}",
            found.join("\n"));
}
