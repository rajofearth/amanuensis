fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=app.rc");
        println!("cargo:rerun-if-changed=assets/logos/amanuensis.ico");
        embed_resource::compile("app.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("failed to embed Windows resources");
    }
}
