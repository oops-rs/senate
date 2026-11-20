//! The `senate` command, under the name people type.
//!
//! All of it lives in `senate-bridge`; this package exists so that
//! `cargo install senate` installs Senate's own command and nobody else's.

fn main() -> std::process::ExitCode {
    senate_bridge::cli::main()
}
