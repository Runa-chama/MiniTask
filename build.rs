fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=assets/app.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_manifest_file("app.manifest");
        res.set_icon("assets/app.ico");
        res.set("FileDescription", "MiniTask - Lightweight Task Manager");
        res.set("ProductName", "MiniTask");
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_owned());
        res.set("ProductVersion", &version);
        res.set("FileVersion", &version);
        res.set("OriginalFilename", "minitask.exe");
        if let Err(e) = res.compile() {
            panic!("Failed to compile Windows resources: {e}");
        }
    }
}
