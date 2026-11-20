fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI failed to compile");

    // Explorer/taskbar icon and the version info shown in the file's Properties.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/gitust.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/gitust.ico")
            .set("ProductName", "Gitust")
            .set("FileDescription", "Gitust — native Git & GitHub client")
            .set("OriginalFilename", "gitust.exe")
            .set("InternalName", "gitust");
        res.compile().expect("Couldn't embed the Windows icon/version resource (is the Windows SDK installed?)");
    }
}
