fn main() {
    println!("cargo:rerun-if-env-changed=SENATE_PERSON_TEAM_ID");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rerun-if-changed=src/app_confirmation.c");
        cc::Build::new()
            .file("src/app_confirmation.c")
            .flag("-fblocks")
            .compile("senate_app_confirmation");
    }
}
