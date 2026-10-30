// Embeds the app icon and product details in the Windows executable, so
// Explorer, the Start menu, shortcuts and Task Manager show LoudTrace.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icons/LoudTrace.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if !cfg!(windows) {
        // The resource compiler comes with the Windows SDK.
        println!("cargo:warning=Building for Windows on another OS: the executable has no icon.");
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon("assets/icons/LoudTrace.ico")
        .set("FileDescription", "LoudTrace")
        .set("ProductName", "LoudTrace");
    if let Err(error) = resource.compile() {
        panic!("Could not embed the Windows icon: {error}");
    }
}
