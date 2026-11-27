fn main() {
    println!("cargo:rerun-if-env-changed=SENATE_PERSON_TEAM_ID");
    // A Windows main thread gets 1 MiB of stack, Unix 8 MiB; the CLI's
    // futures are sized for the latter.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
        println!("cargo:rustc-link-arg-tests=/STACK:8388608");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=Security");
        println!("cargo:rustc-link-lib=framework=ApplicationServices");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=LocalAuthentication");
        println!("cargo:rerun-if-changed=src/app_confirmation.c");
        println!("cargo:rerun-if-changed=src/presence.m");
        cc::Build::new()
            .file("src/app_confirmation.c")
            .flag("-fblocks")
            .compile("senate_app_confirmation");
        cc::Build::new()
            .file("src/presence.m")
            .flag("-fobjc-arc")
            .compile("senate_presence");
    }
}
