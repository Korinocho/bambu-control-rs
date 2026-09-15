fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Bambu Control");
        res.set("FileDescription", "Bambu Control — LAN printer panel");
        let _ = res.compile();
    }
}
