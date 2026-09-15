fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=assets/icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Bambu Control");
        res.set("FileDescription", "Bambu Control — LAN printer panel");
        res.set("LegalCopyright", "Copyright (c) 2026 Korinocho - MIT OR Apache-2.0");
        // Independent project; the name only identifies compatible hardware.
        res.set("CompanyName", "Bambu Control (independent project)");
        // This used to be swallowed, so a failed resource build silently shipped an
        // exe with no icon and no version info.
        if let Err(e) = res.compile() {
            panic!("embedding the Windows resource failed: {e}");
        }
    }
}
