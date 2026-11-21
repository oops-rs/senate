//! Telling a database Senate made from one it did not.
//!
//! Senate's Postgres migrations are numbered with the same versions as some
//! other schemas (another application's, say) while holding different SQL, so
//! pointing Senate at such a database ends, left to sqlx, in a checksum
//! mismatch that says nothing a person can act on. Before any migration runs,
//! [`PgStore::migrate`](crate::PgStore::migrate) and
//! [`SqliteStore::open`](crate::SqliteStore::open) look at what the database
//! holds, reading only, and refuse with [`LogError::ForeignDatabase`] when it
//! is not Senate's.
//!
//! The judgement itself is pure ([`pg_refusal`], [`sqlite_refusal`]) so it is
//! tested without a server.

use crate::error::LogError;

/// The columns of `events` that Senate's schema defines, in both stores.
pub(crate) const EVENT_COLUMNS: &[&str] = &[
    "committee",
    "seq",
    "id",
    "ts",
    "kind",
    "from_addr",
    "thread",
    "envelope",
];

/// Every table Senate's schema creates, in both stores.
pub(crate) const SENATE_TABLES: &[&str] = &[
    "log_meta",
    "committee_heads",
    "events",
    "cursors",
    "mirrors",
    "heartbeats",
];

/// The migrations table sqlx keeps.
pub(crate) const MIGRATIONS_TABLE: &str = "_sqlx_migrations";

/// What sqlx's migrations table holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Recorded {
    /// There is no such table.
    Absent,
    /// There is a table of that name, but not one sqlx would keep.
    Odd,
    /// `(version, checksum)` of each recorded migration.
    Rows(Vec<(i64, Vec<u8>)>),
}

/// What a Postgres database holds, as far as Senate's schema is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PgFound {
    pub recorded: Recorded,
    /// The columns of `events`, when there is such a table.
    pub events: Option<Vec<String>>,
    /// Which of [`SENATE_TABLES`] exist.
    pub tables: Vec<String>,
}

pub(crate) fn foreign(detail: impl Into<String>) -> LogError {
    LogError::ForeignDatabase(detail.into())
}

fn missing_event_columns(events: &[String]) -> Vec<&'static str> {
    EVENT_COLUMNS
        .iter()
        .copied()
        .filter(|c| !events.iter().any(|have| have == c))
        .collect()
}

/// Why this Postgres database is not Senate's, or `None` when it is Senate's
/// (any version Senate can carry on from) or holds none of Senate's tables
/// yet. `ours` is `(version, checksum)` of each embedded migration.
pub(crate) fn pg_refusal(found: &PgFound, ours: &[(i64, Vec<u8>)]) -> Option<String> {
    match &found.recorded {
        Recorded::Odd => {
            return Some(format!(
                "its `{MIGRATIONS_TABLE}` table is not the one Senate's migrations keep"
            ));
        }
        Recorded::Rows(rows) => {
            for (version, checksum) in rows {
                match ours.iter().find(|(v, _)| v == version) {
                    None => {
                        return Some(format!(
                            "it records migration {version}, which this Senate does not have"
                        ));
                    }
                    Some((_, expected)) if expected != checksum => {
                        return Some(format!(
                            "its record of migration {version} is for different SQL than Senate's"
                        ));
                    }
                    Some(_) => {}
                }
            }
        }
        Recorded::Absent => {
            // Without a record of Senate's migrations, none of Senate's
            // tables should be there: they are someone else's.
            if !found.tables.is_empty() {
                return Some(format!(
                    "it has tables Senate creates ({}) and no record of Senate's migrations",
                    found.tables.join(", ")
                ));
            }
        }
    }
    let events = found.events.as_deref()?;
    let missing = missing_event_columns(events);
    (!missing.is_empty()).then(|| {
        format!(
            "its `events` table has no column {}",
            missing
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// Why this SQLite file is not a Senate log, or `None`. `user_version` counts
/// the migrations applied, `tables` are the file's user tables, `events` the
/// columns of its `events` table when it has one, `known` how many
/// migrations this Senate has.
pub(crate) fn sqlite_refusal(
    user_version: i64,
    tables: &[String],
    events: Option<&[String]>,
    known: usize,
) -> Option<String> {
    if user_version == 0 {
        // A file Senate has not touched is empty; one with tables is
        // someone else's.
        return (!tables.is_empty()).then(|| {
            format!(
                "it is a SQLite file with tables of its own ({})",
                tables.join(", ")
            )
        });
    }
    if usize::try_from(user_version).map_or(true, |v| v > known) {
        return Some(format!(
            "it is at schema version {user_version}, which this Senate does not know"
        ));
    }
    let Some(events) = events else {
        return Some("it has no `events` table".to_owned());
    };
    let missing = missing_event_columns(events);
    (!missing.is_empty()).then(|| {
        format!(
            "its `events` table has no column {}",
            missing
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ours() -> Vec<(i64, Vec<u8>)> {
        vec![(1, vec![1, 1]), (2, vec![2, 2])]
    }

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    fn senate_events() -> Vec<String> {
        cols(EVENT_COLUMNS)
    }

    fn found(recorded: Recorded, events: Option<Vec<String>>, tables: &[&str]) -> PgFound {
        PgFound {
            recorded,
            events,
            tables: cols(tables),
        }
    }

    #[test]
    fn a_database_with_nothing_of_senates_is_fresh() {
        assert_eq!(
            pg_refusal(&found(Recorded::Absent, None, &[]), &ours()),
            None
        );
    }

    #[test]
    fn a_database_senate_made_is_its_own_at_any_point_of_its_migrations() {
        let events = Some(senate_events());
        for rows in [
            vec![],
            vec![(1, vec![1, 1])],
            vec![(1, vec![1, 1]), (2, vec![2, 2])],
        ] {
            assert_eq!(
                pg_refusal(
                    &found(
                        Recorded::Rows(rows),
                        events.clone(),
                        &["events", "log_meta"]
                    ),
                    &ours()
                ),
                None
            );
        }
    }

    #[test]
    fn a_recorded_version_with_another_checksum_is_foreign() {
        let reason = pg_refusal(
            &found(
                Recorded::Rows(vec![(1, vec![9, 9])]),
                Some(senate_events()),
                &[],
            ),
            &ours(),
        )
        .unwrap();
        assert!(reason.contains("migration 1"), "{reason}");
        assert!(reason.contains("different SQL"), "{reason}");
    }

    #[test]
    fn a_recorded_version_senate_does_not_have_is_foreign() {
        let reason = pg_refusal(
            &found(
                Recorded::Rows(vec![(1, vec![1, 1]), (7, vec![7])]),
                None,
                &[],
            ),
            &ours(),
        )
        .unwrap();
        assert!(reason.contains("migration 7"), "{reason}");
        assert!(reason.contains("does not have"), "{reason}");
    }

    #[test]
    fn an_events_table_without_senates_columns_is_foreign() {
        let theirs = cols(&["topic", "seq", "id", "ts", "kind", "from_addr", "envelope"]);
        let reason = pg_refusal(
            &found(Recorded::Rows(vec![(1, vec![1, 1])]), Some(theirs), &[]),
            &ours(),
        )
        .unwrap();
        assert!(reason.contains("`committee`"), "{reason}");
        assert!(reason.contains("`thread`"), "{reason}");
    }

    #[test]
    fn senates_tables_with_no_record_of_its_migrations_are_foreign() {
        let reason = pg_refusal(
            &found(Recorded::Absent, Some(senate_events()), &["events"]),
            &ours(),
        )
        .unwrap();
        assert!(reason.contains("no record"), "{reason}");
    }

    #[test]
    fn a_migrations_table_of_another_shape_is_foreign() {
        assert!(pg_refusal(&found(Recorded::Odd, None, &[]), &ours()).is_some());
    }

    #[test]
    fn a_sqlite_file_is_senates_when_empty_or_at_a_known_version_with_its_columns() {
        assert_eq!(sqlite_refusal(0, &[], None, 1), None);
        let events = senate_events();
        assert_eq!(
            sqlite_refusal(1, &cols(&["events", "log_meta"]), Some(&events), 1),
            None
        );
    }

    #[test]
    fn a_sqlite_file_of_something_else_is_foreign() {
        assert!(sqlite_refusal(0, &cols(&["notes"]), None, 1).is_some());
        assert!(sqlite_refusal(3, &[], None, 1).is_some());
        assert!(sqlite_refusal(1, &cols(&["notes"]), None, 1).is_some());
        let theirs = cols(&["topic", "seq"]);
        assert!(sqlite_refusal(1, &cols(&["events"]), Some(&theirs), 1).is_some());
    }
}
