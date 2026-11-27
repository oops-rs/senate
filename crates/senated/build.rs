fn main() {
    // A Windows main thread gets 1 MiB of stack, Unix 8 MiB; the daemon's
    // futures are sized for the latter.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
}
