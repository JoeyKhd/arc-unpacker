fn main() {
    // The GUI asks for administrator rights at launch: the repack codecs it runs create
    // Global\ shared memory, which needs them (the original installers run elevated too).
    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if windows_msvc {
        println!("cargo:rustc-link-arg-bin=arc-unpack-gui=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bin=arc-unpack-gui=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'");
        // The uninstaller asks for administrator rights itself, only when it needs them; its
        // dialogs need the current common controls.
        println!("cargo:rustc-link-arg-bin=arc-unpack-uninstall=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=arc-unpack-uninstall=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
    // The icon of both programs (assets/icon.ico, made from docs/logo.svg).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile_for("assets/app.rc", ["arc-unpack", "arc-unpack-gui", "arc-unpack-uninstall"], embed_resource::NONE).manifest_optional().expect("compile assets/app.rc");
        println!("cargo:rerun-if-changed=assets/app.rc");
        println!("cargo:rerun-if-changed=assets/icon.ico");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ARC_UNPACK_EMBED_DIR");
}
