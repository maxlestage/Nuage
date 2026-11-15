//! On Windows, `nuage-editor.exe` carries its icon (Explorer, the taskbar), the name and
//! version its Properties show, and a manifest: it runs with the rights of the person who
//! opens it. Elsewhere this does nothing (macOS and Linux packages carry their own icons).

fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=packaging/windows");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }
    let dir =
        std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default()).join("packaging/windows");
    // asInvoker: the editor runs with the rights of the person who opens it, never elevated
    // (only Windows' own cmd.exe and schtasks.exe are, src/local/windows.rs). Common Controls
    // 6: its message box is drawn as the system's other windows are.
    let manifest = std::fs::read_to_string(dir.join("nuage-editor.manifest"))?;
    let icon = dir.join("nuage-editor.ico");
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(&icon.to_string_lossy())
        .set("ProductName", "Nuage DB Editor")
        .set("FileDescription", "Nuage DB Editor")
        .set("InternalName", "nuage-editor")
        .set("OriginalFilename", "nuage-editor.exe")
        .set("LegalCopyright", "Maxime Nathan Lestage")
        .set_manifest(&manifest);
    // The program only: the test executables stay without resources.
    resource.compile_for("nuage-editor")
}
