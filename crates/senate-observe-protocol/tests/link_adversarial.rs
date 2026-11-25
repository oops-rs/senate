//! Adversarial inputs for the `senate://open/v1/` grammar (security review).

use senate_observe_protocol::link::{AppLink, Target};

const P: &str = "senate://open/v1/";

fn rejects(suffix: &str) {
    let url = format!("{P}{suffix}");
    assert!(url.parse::<AppLink>().is_err(), "accepted {url}");
}

fn accepts(suffix: &str) -> AppLink {
    let url = format!("{P}{suffix}");
    url.parse::<AppLink>()
        .unwrap_or_else(|_| panic!("rejected {url}"))
}

#[test]
fn rejects_scheme_authority_and_structure_variants() {
    for url in [
        "SENATE://open/v1/desk",
        "senate://OPEN/v1/desk",
        "senate://open/V1/desk",
        "senate:open/v1/desk",
        "senate:///open/v1/desk",
        "senate://open//v1/desk",
        "senate://open/v1//desk",
        "senate://open/v1/desk/",
        "senate://open/v1/",
        "senate://open/v1",
        " senate://open/v1/desk",
        "senate://open/v1/desk ",
        "senate://open/v1/desk\n",
        "senate://open/v1/desk#",
        "senate://open/v1/desk?",
        "senate://open/v1/desk?senate=",
        "senate://open/v1/desk?senate=id%3Aa#f",
        "senate://open/v1/desk?senate=id%3Aa&",
        "senate://open/v1/desk?senate=id%3Aa?senate=id%3Ab",
        "senate://open/v1/desk?%73enate=id%3Aa",
        "senate://open/v1/desk?SENATE=id%3Aa",
        "senate://open/v1/desk?senate=ID%3Aa",
        "senate://open/v1/desk?senate=id%3A",
        "senate://open/v1/desk?senate=id%3Aa%2Fb",
        "senate://open/v1/desk?senate=%2F",
        "senate://open/v1/desk?senate=%2Fteam%2F",
        "senate://open/v1/desk?senate=%2F%2Fteam",
        "senate://open/v1/desk?senate=%2Fteam%2F.",
        "senate://open/v1/desk?senate=%2Fteam%2F..%2Fx",
        "senate://open/v1/desk?senate=%2Fteam%5Cx",
        "senate://open/v1/desk?senate=%2FTeam",
        "senate://open/v1/desk?senate=%252Fteam",
        "senate://open/v1/Desk",
        "senate://open/v1/%64esk",
        "senate://open/v1/intent/desk",
        "intent://open/v1/desk#Intent;scheme=senate;end",
    ] {
        assert!(url.parse::<AppLink>().is_err(), "accepted {url}");
    }
    let depth17 = "%2Fa".repeat(17);
    rejects(&format!("desk?senate={depth17}"));
    let depth16 = "%2Fa".repeat(16);
    accepts(&format!("desk?senate={depth16}"));
}

#[test]
fn rejects_escapes_separators_unicode_and_controls() {
    for suffix in [
        "committee/%",
        "committee/%2",
        "committee/%G1",
        "committee/%%32%30",
        "committee/a%2Fb",
        "committee/a%2fb",
        "committee/a%5Cb",
        "committee/a%25",
        "committee/a%2525",
        "committee/%2E",
        "committee/%2e%2e",
        "committee/..",
        "committee/.",
        "committee/A",
        "committee/a_b",
        "committee/a%00",
        "committee/a%7F",
        "committee/%E2%80%AEa", // RLO bidi override
        "committee/%D0%B0",     // Cyrillic a (confusable)
        "committee/%EF%BC%A1",  // fullwidth A
        "committee/%C3%A9",
        "committee/%ED%A0%80",    // lone surrogate
        "committee/%F4%90%80%80", // > U+10FFFF
        "committee/%C0%AE",       // overlong '.'
        "senator/a%3A%3Ab%3A%3Ac",
        "senator/a%3A%3A",
        "senator/%3A%3Ab",
        "senator/a%3Ab",
        "senator/a%3A%3A%3Ab",
        "senator/speaker",
        "session/a%3A%3Ab%3A%3Ac",
        "session/agy%3A",
        "session/acp%3A",
        "session/xyz%3Aabc",
        "session/agy%3Aacp%3Aabc",
        "session/a%20b",
        "session/a%2Bb",
        "history/obs-a%2F..",
        "history/%2E%2E",
        "history/a%3A%3Ab",
    ] {
        rejects(suffix);
    }
}

#[test]
fn seq_must_be_canonical_nonzero_u64() {
    for seq in [
        "0",
        "00",
        "01",
        "%30",
        "%301",
        "+1",
        "%2B1",
        "-1",
        "%2D1",
        "1.0",
        "1e3",
        "0x1",
        " 1",
        "%201",
        "1%20",
        "18446744073709551616",
        "99999999999999999999999",
    ] {
        rejects(&format!("thread/senate/{seq}"));
        rejects(&format!("desk/senate/{seq}"));
    }
    accepts("thread/senate/1");
    accepts("desk/senate/18446744073709551615");
}

#[test]
fn session_selector_length_boundary() {
    let ok = "a".repeat(256);
    accepts(&format!("session/{ok}"));
    rejects(&format!("session/{}", "a".repeat(257)));
    // The 256-byte limit covers the runtime prefix.
    accepts(&format!("session/agy%3A{}", "a".repeat(252)));
    rejects(&format!("session/agy%3A{}", "a".repeat(253)));
    // Overall URL limit.
    let long = format!("{P}desk?senate=id%3A{}", "a".repeat(2048));
    assert!(long.parse::<AppLink>().is_err());
}

#[test]
fn documents_lenient_but_harmless_acceptances() {
    // Over-encoded unreserved bytes decode once to the same canonical value.
    let link = accepts("committee/%73enate");
    assert_eq!(link.target(), &Target::Committee("senate".into()));
    assert_eq!(link.to_string(), format!("{P}committee/senate"));
    // A prefixed runtime ID may be all dots; the app must treat it as an
    // opaque registry key, never a path.
    let link = accepts("session/agy%3A..");
    assert_eq!(link.target(), &Target::Session("agy:..".into()));
    let link = accepts("history/...");
    assert_eq!(link.target(), &Target::HistoryEntry("...".into()));
    // A value beginning with '-' is not a slug, but session IDs may start
    // with it; the CLI must never pass such a value as a launcher argument
    // without the full `senate://` prefix.
    accepts("session/-n");
}

#[test]
fn generated_links_reparse_and_never_carry_invalid_targets() {
    // for_senate validates by reparsing; a raw path or NUL never becomes a link.
    for target in [
        Target::Committee("../etc".into()),
        Target::Session("/Users/x/.claude/projects/a.jsonl".into()),
        Target::HistoryEntry("a b".into()),
        Target::Senator("a::b::c".into()),
    ] {
        assert!(AppLink::for_senate(target, "demo").is_err());
    }
    assert!(AppLink::for_senate(Target::Desk, "a/b").is_err());
    assert!(AppLink::for_senate(Target::Desk, "").is_err());
    assert!(AppLink::for_senate(Target::Desk, &"a".repeat(129)).is_err());
    for target in [
        Target::Thread("senate".into(), "18446744073709551615".parse().unwrap()),
        Target::Senator("senate::agent-1".into()),
        Target::HistoryEntry("legacy-entry".into()),
    ] {
        let link = AppLink::for_senate(target, "demo").unwrap();
        let again: AppLink = link.to_string().parse().unwrap();
        assert_eq!(again, link);
    }
}

#[test]
fn address_keywords_are_literal_only_in_committee_positions() {
    for name in ["senator", "speaker", "chair", "committee", "guest"] {
        for (route, target) in [
            (format!("committee/{name}"), Target::Committee(name.into())),
            (
                format!("thread/{name}/42"),
                Target::Thread(name.into(), "42".parse().unwrap()),
            ),
            (
                format!("desk/{name}/42"),
                Target::DeskItem(name.into(), "42".parse().unwrap()),
            ),
            (
                format!("session/{name}%3A%3Aworker"),
                Target::Session(format!("{name}::worker")),
            ),
            (
                format!("senator/{name}%3A%3Aworker"),
                Target::Senator(format!("{name}::worker")),
            ),
        ] {
            assert_eq!(accepts(&route).target(), &target);
            assert_eq!(
                AppLink::for_senate(target, "demo").unwrap().to_string(),
                format!("{P}{route}?senate=id%3Ademo")
            );
        }
        rejects(&format!("senator/{name}"));
        rejects(&format!("senator/work%3A%3A{name}"));
        rejects(&format!("session/work%3A%3A{name}"));
        rejects(&format!("desk?senate=%2F{name}"));
    }
}
