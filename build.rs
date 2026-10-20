fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
    println!("cargo:rerun-if-changed=app.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}\\app.manifest", std::env::var("CARGO_MANIFEST_DIR").unwrap());
    }
}
