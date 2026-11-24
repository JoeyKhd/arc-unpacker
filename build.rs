fn main() {
    // The GUI asks for administrator rights at launch: the repack codecs it runs create
    // Global\ shared memory, which needs them (the original installers run elevated too).
    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if windows_msvc {
        println!("cargo:rustc-link-arg-bin=arc-unpack-gui=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bin=arc-unpack-gui=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ARC_UNPACK_EMBED_DIR");
}
