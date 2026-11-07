pub fn build() {
    println!("cargo:rerun-if-changed=../native/macos-admin.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let version = format!(
            "\"{}\"",
            std::env::var("CARGO_PKG_VERSION").expect("Cargo-Paketversion fehlt")
        );
        cc::Build::new()
            .file("../native/macos-admin.m")
            .define("DD_METADATA_BUILD_VERSION", Some(version.as_str()))
            .flag("-fobjc-arc")
            .flag("-fblocks")
            .flag("-mmacosx-version-min=14.0")
            .warnings(true)
            .compile("doppeldecker_metadata_admin");
        for framework in ["Foundation", "Security", "ServiceManagement"] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    }
}
