use std::error::Error;

use schemars::{Schema, schema_for};
use senate_observe_protocol::{
    ErrorResponse, Event, EventFrame, EventHeader, ResetFrame, SenatorState, Seq, Snapshot,
    link::{AppLink, Target},
};
use serde::Deserialize;
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const STATE: &str = include_str!("../../../docs/observe/v1/fixtures/state.json");
const UNSEAT: &str = include_str!("../../../docs/observe/v1/fixtures/unseat.sse.json");
const RESET: &str = include_str!("../../../docs/observe/v1/fixtures/reset.sse.json");

#[test]
fn schema_files_match_rust_types() -> TestResult {
    let cases: [(Schema, &str); 4] = [
        (
            schema_for!(Snapshot),
            include_str!("../../../docs/observe/v1/snapshot.schema.json"),
        ),
        (
            schema_for!(EventFrame),
            include_str!("../../../docs/observe/v1/event.schema.json"),
        ),
        (
            schema_for!(ResetFrame),
            include_str!("../../../docs/observe/v1/reset.schema.json"),
        ),
        (
            schema_for!(ErrorResponse),
            include_str!("../../../docs/observe/v1/error.schema.json"),
        ),
    ];
    for (generated, checked_in) in cases {
        assert_eq!(
            format!("{}\n", serde_json::to_string_pretty(&generated)?),
            checked_in
        );
    }
    Ok(())
}

#[test]
fn state_round_trips_elders_and_committee_local_sequences() -> TestResult {
    let state: Snapshot = serde_json::from_str(STATE)?;
    assert_eq!(
        serde_json::to_value(&state)?,
        serde_json::from_str::<Value>(STATE)?
    );
    assert!(state.committees[0].archived);
    assert_eq!(state.senators[0].state, SenatorState::Away);
    assert_eq!(state.senators[1].state, SenatorState::Stopped);
    assert!(
        state
            .senators
            .iter()
            .all(|seat| seat.last_active_at.is_some())
    );
    assert_eq!(state.threads[0].root_seq, state.threads[1].root_seq);
    assert_ne!(state.threads[0].committee_id, state.threads[1].committee_id);
    assert_eq!(state.threads[0].root_seq.0.get(), u64::MAX);

    let raw: String = serde_json::from_str(UNSEAT)?;
    let lines: Vec<_> = raw.lines().collect();
    let [id, data, ""] = lines.as_slice() else {
        return Err("expected one terminated SSE event".into());
    };
    let event: EventFrame =
        serde_json::from_str(data.strip_prefix("data: ").ok_or("missing data")?)?;
    assert_eq!(id.strip_prefix("id: "), Some(event.cursor.as_str()));
    let Event::StatePatch(patch) = event.event else {
        return Err("expected state patch".into());
    };
    let removals = patch.remove.ok_or("expected removed seat")?;
    assert_eq!(removals.senators[0].id, state.senators[1].id);
    assert_eq!(
        removals.senators[0].committee_id,
        state.senators[1].committee_id
    );
    Ok(())
}

#[test]
fn resets_do_not_advance_and_unknown_events_can_advance() -> TestResult {
    let raw: String = serde_json::from_str(RESET)?;
    let json = raw.strip_prefix("data: ").ok_or("missing reset data")?;
    let header: EventHeader = serde_json::from_str(json)?;
    assert_eq!(header.kind, "stream.reset");
    assert_eq!(header.cursor, None);
    let _: ResetFrame = serde_json::from_str(json)?;

    let future = r#"{"schema_version":1,"cursor":"AAAAAAAAAAAAAAAAAAAAAA","kind":"future.event","data":{"new_field":true}}"#;
    let header: EventHeader = serde_json::from_str(future)?;
    assert_eq!(header.kind, "future.event");
    assert!(header.cursor.is_some());
    assert!(
        serde_json::from_str::<EventHeader>(
            &future.replace("\"schema_version\":1", "\"schema_version\":2")
        )
        .is_err()
    );
    assert_eq!(
        serde_json::from_str::<SenatorState>(r#""new_state""#)?,
        SenatorState::Unknown
    );
    for seq in ["0", "01", "+1", "-1", "18446744073709551616"] {
        assert!(seq.parse::<Seq>().is_err(), "{seq}");
    }
    assert!(serde_json::from_str::<Seq>("42").is_err());
    Ok(())
}

#[derive(Deserialize)]
struct LinkCases {
    valid: Vec<(String, String)>,
    invalid: Vec<String>,
}

#[test]
fn deep_links_match_the_public_fixtures() -> TestResult {
    let cases: LinkCases =
        serde_json::from_str(include_str!("../../../docs/observe/v1/fixtures/links.json"))?;
    for (input, canonical) in cases.valid {
        let link: AppLink = format!("senate://open/v1/{input}").parse()?;
        assert_eq!(link.to_string(), format!("senate://open/v1/{canonical}"));
    }
    for input in cases.invalid {
        assert!(input.parse::<AppLink>().is_err(), "{input}");
    }
    for (route, canonical) in [
        ("committee/senate", "committee/senate"),
        ("session/senate::agent", "session/senate%3A%3Aagent"),
        ("thread/senate/42", "thread/senate/42"),
        ("history/agy:abc", "history/agy%3Aabc"),
        ("inbox", "inbox"),
    ] {
        let link = AppLink::from_route(route)?;
        assert_eq!(link.to_string(), format!("senate://open/v1/{canonical}"));
        assert_eq!(link.senate(), None);
    }
    for route in [
        "",
        "approve/42",
        "session/senate%3A%3Aagent",
        "thread/senate/0",
        "committee/a/b",
        "inbox?senate=id%3Ademo",
        "committee/../desk",
    ] {
        assert!(AppLink::from_route(route).is_err(), "{route}");
    }
    let link = AppLink::for_senate(Target::Senator("senate::agent".into()), "demo")?;
    assert_eq!(link.senate(), Some("id:demo"));
    assert_eq!(
        link.to_string(),
        "senate://open/v1/senator/senate%3A%3Aagent?senate=id%3Ademo"
    );
    Ok(())
}
