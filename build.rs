// Embed the shared application icon so Explorer and shortcuts use the same branding.
fn main() {
    println!("cargo:rerun-if-changed=assets/branding/app.ico");

    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/branding/app.ico")
            .set("ProductName", "CodexProfiler")
            .set("FileDescription", "CodexProfiler")
            .compile()
            .expect("Could not embed the application icon; install Windows SDK rc.exe or MinGW windres.exe");
    }
}
