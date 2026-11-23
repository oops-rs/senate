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

/// Read the protected history even while the remote listener is disabled.
pub(crate) fn history(paths: &crate::Paths) -> anyhow::Result<Vec<History>> {
    Ok(super::authority_for(paths)?.read()?.history)
}

pub(crate) fn history_line(entry: &History) -> Option<String> {
    let action = match entry.action.as_str() {
        "enable" => "The remote listener was enabled",
        "disable" => "The remote listener was disabled",
        "enroll" => "A remote device was enrolled",
        "revoke" => "A remote device was revoked",
        "enrollment_rejected" => "A remote enrollment was rejected",
        _ => return None,
    };
    let time = chrono::DateTime::from_timestamp(entry.at, 0)?;
    Some(format!("{action} at {time}."))
}

pub(crate) fn latest_history(paths: &crate::Paths) -> Option<String> {
    history(paths).ok()?.iter().rev().find_map(history_line)
}

pub(crate) async fn notify_history(
    paths: crate::Paths,
    notifier: std::sync::Arc<dyn crate::notify::Notifier>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let cursor = paths.home().join("remote-history-seen");
    let mut seen: u64 = super::read(&cursor).unwrap_or_default();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tick.tick() => {
                let Ok(mut entries) = history(&paths) else { continue };
                entries.sort_by_key(|entry| entry.generation);
                let previous = seen;
                for entry in entries {
                    if entry.generation <= seen {
                        continue;
                    }
                    if let Some(line) = history_line(&entry) {
                        let notifier = notifier.clone();
                        match tokio::task::spawn_blocking(move || notifier.post("Senate remote administration", &line)).await {
                            Ok(Ok(())) => {}
                            _ => {
                                tracing::warn!("could not post a remote administration notification");
                                break;
                            }
                        }
                    }
                    seen = entry.generation;
                }
                if seen != previous && let Ok(file) = super::durable(&cursor, &seen) {
                    let _ = file.persist(&cursor);
                }
            }
        }
    }
}
