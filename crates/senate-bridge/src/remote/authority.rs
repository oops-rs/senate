//! Bridge-side access to the minimal administrator crate.
pub(crate) use senate_remote_admin::*;

pub(crate) fn installation_checks() -> Vec<crate::doctor::Check> {
    use crate::doctor::Check;

    fn missing(error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    }
    let helper = match installed_helper_hash() {
        Ok(sha) => match option_env!("SENATE_REMOTE_ADMIN_SHA256") {
            Some(expected) if sha == expected => Check::ok(
                "remote.helper",
                format!("root-owned 0755, SHA-256 {sha} matches this build"),
            ),
            Some(expected) => Check::fail(
                "remote.helper",
                format!("SHA-256 {sha} differs from expected {expected}"),
                None::<String>,
            ),
            None => Check::warn(
                "remote.helper",
                format!(
                    "root-owned 0755, SHA-256 {sha}. Compare it with a trusted release checksum"
                ),
                None::<String>,
            ),
        },
        Err(error) if missing(&error) => Check::ok("remote.helper", "not installed"),
        Err(error) => Check::fail("remote.helper", error.to_string(), None::<String>),
    };
    let authority = match check_store() {
        Ok(()) => Check::ok(
            "remote.authority",
            "root-owned directories and profile, no group or other write access",
        ),
        Err(error) if missing(&error) => Check::ok("remote.authority", "not installed"),
        Err(error) => Check::fail("remote.authority", error.to_string(), None::<String>),
    };
    vec![helper, authority]
}
