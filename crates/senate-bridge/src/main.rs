//! The `senate` binary. The command itself is [`senate_bridge::cli`], so the
//! `senate` package can install the very same one.

fn main() -> std::process::ExitCode {
    senate_bridge::cli::main()
}
