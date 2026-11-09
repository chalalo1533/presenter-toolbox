// Embeds the magnifier icon and the version details shown in Explorer → Properties.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico")
            .set("ProductName", "Presenter Toolbox")
            .set("FileDescription", "Presenter Toolbox")
            .set("CompanyName", "chalalo")
            .set("LegalCopyright", "© 2026 Gonzalo Pérez Correa (@chalalo)")
            .set("OriginalFilename", "presenter_toolbox.exe");
        if let Err(e) = res.compile() {
            println!("cargo:warning=no se pudo incrustar el icono: {e}");
        }
    }
}
