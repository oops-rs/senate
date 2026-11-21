//! On macOS, compiles the two-function C shim the unified-log layer calls
//! (`src/oslog/shim.c`): the os_log API is macros only.

fn main() {
    println!("cargo:rerun-if-changed=src/oslog/shim.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("src/oslog/shim.c")
            .warnings(true)
            .compile("senate_oslog");
    }
}
