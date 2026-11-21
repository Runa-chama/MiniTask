fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=assets/app.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_manifest_file("app.manifest");
        res.set_icon("assets/app.ico");
        res.set("FileDescription", "MiniTask - Lightweight Task Manager");
        res.set("ProductName", "MiniTask");
        res.set("ProductVersion", "0.1.0");
        res.set("FileVersion", "0.1.0");
        res.set("OriginalFilename", "minitask.exe");
        if let Err(e) = res.compile() {
            panic!("Failed to compile Windows resources: {e}");
        }
    }
}
