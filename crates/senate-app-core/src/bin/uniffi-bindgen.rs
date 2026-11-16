//! UniFFI's bindings generator, at the exact version the core is built
//! with. `apps/macos/scripts/build-core.sh` runs it:
//!
//! ```sh
//! cargo run -p senate-app-core --features bindgen --bin uniffi-bindgen -- \
//!     generate --library target/debug/libsenate_app_core.a --language swift --out-dir apps/macos/Generated
//! ```

fn main() {
    uniffi::uniffi_bindgen_main();
}
