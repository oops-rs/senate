//! The built-in session's configuration: the layers (defaults, the main
//! configuration's `[model]` and compatibility table, `secretary.toml`, a
//! committee's file), reloading without a restart, a broken file keeping
//! the last good configuration, and what `secretary show` prints.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use senate_core::{CommitteeId, Locale};

use super::rig::{committee, refund};
use crate::secretary::{
    BaseLayer, Duty, ModelCheck, ModelSettings, SecretaryConfigs, Source, table_of_config,
};
use crate::{SpeakerConfig, SpeakerMode};

/// A temporary Senate home with the files written by the test.
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("a temp dir"),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path().join(name)
    }

    /// Write `name`, and make its modification time differ from any
    /// earlier write's even on a coarse clock: the reload compares time
    /// and size.
    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.file(name);
        let before = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        std::fs::write(&path, text).expect("write");
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open");
        let at = before.map_or_else(SystemTime::now, |b| b + Duration::from_secs(2));
        file.set_modified(at).expect("mtime");
        path
    }

    fn remove(&self, name: &str) {
        std::fs::remove_file(self.file(name)).expect("remove");
    }

    fn data(&self) -> PathBuf {
        self.path().join("data")
    }
}

/// Models are complete when they name one of the scripted providers.
fn scripted_check() -> ModelCheck {
    Arc::new(|m: &ModelSettings| match m.provider.as_deref() {
        Some("alpha" | "beta") => Ok(()),
        Some(other) => Err(format!("unknown provider {other:?}")),
        None => Err("no provider".into()),
    })
}

fn configs(home: &Home, base: BaseLayer) -> SecretaryConfigs {
    SecretaryConfigs::with_check(Some(home.path().into()), base, scripted_check())
}

fn base(home: &Home) -> BaseLayer {
    BaseLayer::new(home.data(), Locale::En)
        .with_main_file(home.file("senated.toml"))
        .with_model(ModelSettings {
            provider: Some("alpha".into()),
            model: Some("m-1".into()),
            ..ModelSettings::default()
        })
}

fn other() -> CommitteeId {
    committee("billing")
}

#[test]
fn no_file_at_all_is_the_defaults() {
    let home = Home::new();
    for dir in [None, Some(home.path().to_path_buf())] {
        let configs = SecretaryConfigs::new(dir, BaseLayer::new(home.data(), Locale::Zh));
        let effective = configs.refresh(Some(&refund()));
        assert_eq!(effective.name.as_str(), "secretary");
        let mut expected = SpeakerConfig::new(home.data());
        expected.locale = Locale::Zh;
        assert_eq!(effective.speaker, expected);
        assert!(!effective.model_configured);
        assert_eq!(effective.model, None);
        assert_eq!(effective.instructions, None);
        assert_eq!(effective.duties, Duty::ALL.into_iter().collect());
        assert!(configs.problems().is_empty());
        assert!(configs.notes().is_empty());
        assert!(!configs.model_in_force());
    }
}

#[test]
fn secretary_toml_alone() {
    let home = Home::new();
    home.write(
        "secretary.toml",
        r#"name = "clerk"
mode = "agent"
confirmation_ttl_secs = 120
bot_name = "Clerk"
instructions = "Answer in short sentences."
duties = ["route", "forward"]

[model]
model = "m-2"
"#,
    );
    let configs = configs(&home, base(&home));
    assert_eq!(configs.name().as_str(), "clerk");
    let effective = configs.refresh(None);
    assert_eq!(effective.speaker.mode, SpeakerMode::Agent);
    assert_eq!(effective.speaker.confirmation_ttl_secs, 120);
    assert_eq!(effective.speaker.bot_name, "Clerk");
    assert_eq!(
        effective.instructions.as_deref(),
        Some("Answer in short sentences.")
    );
    assert!(effective.may(Duty::Forward));
    assert!(!effective.may(Duty::Cover));
    let model = effective.model.clone().expect("a model");
    assert_eq!(model.provider.as_deref(), Some("alpha"), "from [model]");
    assert_eq!(model.model.as_deref(), Some("m-2"), "from secretary.toml");
    assert_eq!(effective.speaker.model.as_deref(), Some("m-2"));
    assert!(configs.problems().is_empty(), "{:?}", configs.problems());
}

#[test]
fn a_committee_file_overrides_some_keys_and_inherits_the_rest() {
    let home = Home::new();
    home.write(
        "secretary.toml",
        "mode = \"agent\"\nconfirmation_ttl_secs = 120\n[work]\nallowed_repos = [\"example/refund\"]\n",
    );
    home.write(
        "refund-v2.secretary.toml",
        "confirmation_ttl_secs = 30\ninstructions = \"Never promise a date.\"\n[work]\npr_base = \"develop\"\n",
    );
    // Not a committee id: never read.
    home.write("Not A Committee.secretary.toml", "mode = \"loud\"\n");
    let configs = configs(&home, base(&home));

    let refund_config = configs.refresh(Some(&refund()));
    assert_eq!(refund_config.speaker.confirmation_ttl_secs, 30);
    assert_eq!(refund_config.speaker.mode, SpeakerMode::Agent, "inherited");
    let work = refund_config.speaker.work.clone().expect("work");
    assert!(work.allowed_repos.allows("example/refund"), "inherited");
    assert_eq!(work.pr_base, "develop", "this committee's");
    assert_eq!(
        refund_config.instructions.as_deref(),
        Some("Never promise a date.")
    );

    let other_config = configs.refresh(Some(&other()));
    assert_eq!(other_config.speaker.confirmation_ttl_secs, 120);
    assert_eq!(
        other_config.speaker.work.clone().expect("work").pr_base,
        "main"
    );
    assert_eq!(other_config.instructions, None);
    assert!(configs.problems().is_empty(), "{:?}", configs.problems());
}

#[test]
fn a_committee_file_may_not_set_the_name() {
    let home = Home::new();
    home.write(
        "secretary.toml",
        "name = \"clerk\"\nconfirmation_ttl_secs = 120\n",
    );
    let file = home.write(
        "refund-v2.secretary.toml",
        "# refund-v2\nconfirmation_ttl_secs = 30\nname = \"other\"\n",
    );
    let configs = configs(&home, base(&home));
    let effective = configs.refresh(Some(&refund()));
    assert_eq!(effective.name.as_str(), "clerk");
    assert_eq!(
        effective.speaker.confirmation_ttl_secs, 120,
        "the file is refused as a whole"
    );
    let problems = configs.problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].file, file);
    assert_eq!(problems[0].line, Some(3));
    assert!(
        problems[0].message.contains("may not set name"),
        "{problems:?}"
    );
}

#[test]
fn an_edit_takes_effect_without_a_restart() {
    let home = Home::new();
    home.write("secretary.toml", "confirmation_ttl_secs = 120\n");
    let configs = configs(&home, base(&home));
    assert_eq!(
        configs
            .refresh(Some(&refund()))
            .speaker
            .confirmation_ttl_secs,
        120
    );

    home.write(
        "secretary.toml",
        "confirmation_ttl_secs = 300\nmode = \"agent\"\nname = \"clerk\"\n",
    );
    // What was last computed stays until the next refresh…
    assert_eq!(
        configs
            .effective(Some(&refund()))
            .speaker
            .confirmation_ttl_secs,
        120
    );
    // …which reads the change.
    let effective = configs.refresh(Some(&refund()));
    assert_eq!(effective.speaker.confirmation_ttl_secs, 300);
    assert_eq!(effective.speaker.mode, SpeakerMode::Agent);
    assert_eq!(
        effective.name.as_str(),
        "secretary",
        "a new name applies at the next start"
    );
    let restarted =
        SecretaryConfigs::with_check(Some(home.path().into()), base(&home), scripted_check());
    assert_eq!(restarted.name().as_str(), "clerk");
}

#[test]
fn a_broken_edit_keeps_the_last_good_configuration_and_says_where() {
    let home = Home::new();
    home.write("secretary.toml", "confirmation_ttl_secs = 120\n");
    let configs = configs(&home, base(&home));
    assert_eq!(configs.refresh(None).speaker.confirmation_ttl_secs, 120);

    // A key with a value it cannot have, on line 3.
    let file = home.write(
        "secretary.toml",
        "# edited\nconfirmation_ttl_secs = 5\nmode = \"loud\"\n",
    );
    let effective = configs.refresh(None);
    assert_eq!(
        effective.speaker.confirmation_ttl_secs, 120,
        "nothing of the broken file is applied"
    );
    let problems = configs.problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].file, file);
    assert_eq!(problems[0].line, Some(3));

    // Not TOML at all, on line 2.
    home.write("secretary.toml", "confirmation_ttl_secs = 5\nmode = \n");
    assert_eq!(configs.refresh(None).speaker.confirmation_ttl_secs, 120);
    let problems = configs.problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].line, Some(2));

    // Fixed: applied, and the problem is gone.
    home.write("secretary.toml", "confirmation_ttl_secs = 45\n");
    assert_eq!(configs.refresh(None).speaker.confirmation_ttl_secs, 45);
    assert!(configs.problems().is_empty());
}

#[test]
fn a_committee_file_added_and_removed_at_runtime() {
    let home = Home::new();
    let configs = configs(&home, base(&home));
    assert_eq!(
        configs
            .refresh(Some(&refund()))
            .speaker
            .answer_fallback_secs,
        60
    );

    home.write("refund-v2.secretary.toml", "answer_fallback_secs = 10\n");
    assert_eq!(
        configs
            .refresh(Some(&refund()))
            .speaker
            .answer_fallback_secs,
        10
    );
    assert_eq!(
        configs.refresh(Some(&other())).speaker.answer_fallback_secs,
        60,
        "only that committee"
    );

    home.remove("refund-v2.secretary.toml");
    assert_eq!(
        configs
            .refresh(Some(&refund()))
            .speaker
            .answer_fallback_secs,
        60
    );
}

/// An old `[speaker]` table with a model key in it.
fn compat_base(home: &Home) -> BaseLayer {
    let mut old = SpeakerConfig::new(home.data());
    old.answer_fallback_secs = 30;
    old.model = Some("m-old".into());
    let mut base = BaseLayer::new(home.data(), Locale::En)
        .with_main_file(home.file("senated.toml"))
        .with_compat("speaker", table_of_config(&old));
    base.model = Some(ModelSettings {
        provider: Some("alpha".into()),
        ..ModelSettings::default()
    });
    base
}

#[test]
fn an_old_speaker_table_is_read_as_secretary_toml_until_the_file_exists() {
    let home = Home::new();
    let configs = configs(&home, compat_base(&home));
    let effective = configs.refresh(None);
    assert_eq!(effective.speaker.answer_fallback_secs, 30);
    let model = effective.model.clone().expect("a model");
    assert_eq!(model.model.as_deref(), Some("m-old"), "still read");
    assert_eq!(model.provider.as_deref(), Some("alpha"), "[model] over it");
    let notes = configs.notes().join("\n");
    assert!(notes.contains("[speaker] table"), "{notes}");
    assert!(
        notes.contains("read as if it were secretary.toml"),
        "{notes}"
    );
    assert!(notes.contains("top-level [model]"), "{notes}");

    // The file wins: the table's behaviour is no longer read, its model
    // keys still are.
    home.write("secretary.toml", "confirmation_ttl_secs = 90\n");
    let restarted = configs_from(&home, compat_base(&home));
    let effective = restarted.refresh(None);
    assert_eq!(effective.speaker.answer_fallback_secs, 60);
    assert_eq!(effective.speaker.confirmation_ttl_secs, 90);
    assert_eq!(
        effective.model.clone().expect("a model").model.as_deref(),
        Some("m-old")
    );
    let notes = restarted.notes().join("\n");
    assert!(notes.contains("the file wins"), "{notes}");
}

fn configs_from(home: &Home, base: BaseLayer) -> SecretaryConfigs {
    configs(home, base)
}

#[test]
fn a_main_tables_compat_and_model_are_read_and_both_tables_refused() {
    let table: toml::Table = toml::from_str(
        "[model]\nprovider = \"alpha\"\n[secretary]\nname = \"clerk\"\nbrain_deadline_secs = 20\n",
    )
    .expect("toml");
    let base = BaseLayer::of_main_table(&table, "/d", Locale::En).expect("reads");
    let (section, behaviour) = base.compat.clone().expect("compat");
    assert_eq!(section, "secretary");
    assert_eq!(
        behaviour.get("name").and_then(|v| v.as_str()),
        Some("clerk")
    );
    assert_eq!(
        base.compat_model.clone().expect("model keys").deadline_secs,
        Some(20)
    );
    assert_eq!(
        base.model.clone().expect("[model]").provider.as_deref(),
        Some("alpha")
    );

    let both: toml::Table = toml::from_str("[secretary]\n[speaker]\n").expect("toml");
    assert!(BaseLayer::of_main_table(&both, "/d", Locale::En).is_err());
    let bad: toml::Table = toml::from_str("[model]\nprovidr = \"alpha\"\n").expect("toml");
    assert!(BaseLayer::of_main_table(&bad, "/d", Locale::En).is_err());
}

#[test]
fn an_incomplete_override_falls_back_with_a_report() {
    let home = Home::new();
    let file = home.write(
        "refund-v2.secretary.toml",
        "[model]\nprovider = \"gamma\"\nmodel = \"m-9\"\n",
    );
    let configs = configs(&home, base(&home));
    let model = configs
        .refresh(Some(&refund()))
        .model
        .clone()
        .expect("a model");
    assert_eq!(model.provider.as_deref(), Some("alpha"));
    assert_eq!(
        model.model.as_deref(),
        Some("m-1"),
        "the layer below, whole"
    );
    let problems = configs.problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].file, file);
    assert!(
        problems[0].message.contains("the next layer down is used"),
        "{problems:?}"
    );
    assert!(
        problems[0].message.contains("senated.toml [model]"),
        "{problems:?}"
    );
}

#[test]
fn show_names_the_source_of_each_model_key_and_hides_the_key() {
    let home = Home::new();
    // HOME is set wherever the tests run; its value must never be shown.
    let secret = std::env::var("HOME").expect("HOME is set");
    let committee_file = home.write(
        "refund-v2.secretary.toml",
        "confirmation_ttl_secs = 30\n[model]\nprovider = \"beta\"\napi_key_env = \"HOME\"\n",
    );
    let secretary_file = home.write(
        "secretary.toml",
        "mode = \"agent\"\n[model]\nmodel = \"m-2\"\n",
    );
    let configs = configs(&home, base(&home));
    let shown = configs.explain(Some(&refund()));

    assert!(shown.model_in_force);
    let source_of = |key: &str| shown.model_value(key).map(|v| v.source.clone());
    assert_eq!(
        source_of("provider"),
        Some(Source::File(committee_file.clone()))
    );
    assert_eq!(
        source_of("model"),
        Some(Source::File(secretary_file.clone()))
    );
    assert_eq!(
        source_of("api_key_env"),
        Some(Source::File(committee_file.clone()))
    );
    let key = shown.model_value("api_key_env").expect("api_key_env");
    assert_eq!((key.value.as_str(), key.key_set), ("HOME", Some(true)));
    assert_eq!(
        shown
            .value("confirmation_ttl_secs")
            .map(|v| v.source.clone()),
        Some(Source::File(committee_file))
    );
    assert_eq!(
        shown.value("mode").map(|v| v.source.clone()),
        Some(Source::File(secretary_file))
    );
    assert_eq!(
        shown
            .value("answer_fallback_secs")
            .map(|v| v.source.clone()),
        Some(Source::Default)
    );

    // Only [model] configures one here: its key is named by [model].
    let senate = configs.explain(None);
    assert_eq!(
        senate.model_value("provider").map(|v| v.source.clone()),
        Some(Source::Main {
            file: Some(home.file("senated.toml")),
            table: Some("model".into())
        })
    );

    for l in Locale::ALL {
        let text = shown.render(l);
        assert!(!text.contains(&secret), "the key's value is shown:\n{text}");
        assert!(text.contains("[senator::secretary]"), "{text}");
        assert!(text.contains("[refund-v2]"), "{text}");
        assert!(text.contains("refund-v2.secretary.toml"), "{text}");
        crate::tests_support::assert_senate_vocabulary(
            &text.replace(&home.path().display().to_string(), ""),
        );
    }
    assert!(shown.render(Locale::Zh).contains("HOME（已设置）"));
    assert!(shown.render(Locale::En).contains("HOME (set)"));
}

#[test]
fn a_base_url_is_shown_without_its_credentials() {
    let home = Home::new();
    let mut base = base(&home);
    base.model = Some(ModelSettings {
        provider: Some("alpha".into()),
        base_url: Some("https://user:pw@models.example/v1".into()),
        ..ModelSettings::default()
    });
    let shown = configs(&home, base).explain(None);
    assert_eq!(
        shown.model_value("base_url").map(|v| v.value.as_str()),
        Some("\"https://***@models.example/v1\"")
    );
}

#[test]
fn check_all_finds_a_broken_committee_file_nobody_has_acted_on() {
    let home = Home::new();
    home.write("secretary.toml", "confirmation_ttl_secs = 120\n");
    let broken = home.write("refund-v2.secretary.toml", "mode = \"loud\"\n");
    home.write("notes.toml", "mode = \"loud\"\n");
    let configs = configs(&home, base(&home));
    assert!(configs.problems().is_empty(), "nothing read yet");
    let problems = configs.check_all();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].file, broken);
    assert_eq!(problems[0].line, Some(1));
    // Fixed: gone at the next look.
    home.write("refund-v2.secretary.toml", "answer_fallback_secs = 10\n");
    assert!(configs.check_all().is_empty());
}

/// `wire` names how a `base_url` is spoken to: two values, and only with a
/// base URL; it layers like every other key and is part of the connection.
#[test]
fn a_model_wire_is_checked_layered_and_keyed() {
    let gateway = ModelSettings {
        base_url: Some("https://gateway.example.com/v1".into()),
        model: Some("gpt-6.1".into()),
        wire: Some("responses".into()),
        ..ModelSettings::default()
    };
    assert_eq!(gateway.check_basis(), Ok(()));
    assert_eq!(
        crate::secretary::wire_of("responses"),
        Ok(basis::runtime::Wire::Responses)
    );
    assert_eq!(
        crate::secretary::wire_of("chat_completions"),
        Ok(basis::runtime::Wire::ChatCompletions)
    );

    let wrong = ModelSettings {
        wire: Some("grpc".into()),
        ..gateway.clone()
    };
    assert!(wrong.check_basis().unwrap_err().contains("is not one of"));

    let no_url = ModelSettings {
        provider: Some("openai".into()),
        wire: Some("responses".into()),
        ..ModelSettings::default()
    };
    assert!(
        no_url
            .check_basis()
            .unwrap_err()
            .contains("needs a base_url")
    );

    let above = ModelSettings {
        model: Some("gpt-6.1-sol".into()),
        ..ModelSettings::default()
    };
    assert_eq!(above.over(&gateway).wire.as_deref(), Some("responses"));
    let chat = ModelSettings {
        wire: Some("chat_completions".into()),
        ..gateway.clone()
    };
    assert_ne!(chat.connection_key(), gateway.connection_key());
}

/// `[model] fallback = { … }` parses as a second model table, is checked
/// with it (and cannot nest), and makes the third tier: the primary, its
/// one retry, then the fallback on its own connection with the primary's
/// bounds where it says none.
#[test]
fn a_model_fallback_is_parsed_checked_and_tiered() {
    let table: toml::Table = toml::from_str(
        r#"
        [model]
        base_url = "https://gateway.example.com/v1"
        model = "gpt-6.1"
        deadline_secs = 30
        fallback = { provider = "anthropic", model = "claude-sonnet-5-5", triage_deadline_secs = 4 }
        "#,
    )
    .expect("toml");
    let base = BaseLayer::of_main_table(&table, "/tmp/data", Locale::En).expect("base");
    let model = base.model.expect("model");
    assert_eq!(model.check_basis(), Ok(()));

    let tiers = model.tiers();
    let labels: Vec<&str> = tiers.iter().map(|t| t.label).collect();
    assert_eq!(labels, ["primary", "primary-retry", "fallback"]);
    assert_eq!(tiers[0].model, tiers[1].model);
    assert!(tiers[0].model.fallback.is_none());
    let fallback = &tiers[2].model;
    assert_eq!(fallback.model.as_deref(), Some("claude-sonnet-5-5"));
    assert_eq!(fallback.provider.as_deref(), Some("anthropic"));
    // Its own connection: no base_url from the primary; the primary's bounds.
    assert_eq!(fallback.base_url, None);
    assert_eq!(fallback.deadline_secs, Some(30));
    assert_eq!(fallback.triage_deadline_secs, Some(4));
    assert!(fallback.fallback.is_none());

    // Without one: two attempts on the one model.
    let alone = ModelSettings {
        fallback: None,
        ..model.clone()
    };
    assert_eq!(alone.tiers().len(), 2);

    // A broken fallback is the model's problem; so is a nested one.
    let broken = ModelSettings {
        fallback: Some(Box::new(ModelSettings {
            provider: Some("nonesuch".into()),
            ..ModelSettings::default()
        })),
        ..alone.clone()
    };
    assert!(broken.check_basis().unwrap_err().starts_with("fallback: "));
    let nested = ModelSettings {
        fallback: Some(Box::new(model.clone())),
        ..alone.clone()
    };
    assert!(nested.check_basis().unwrap_err().contains("of its own"));

    // A layer above that names no fallback keeps the one below.
    let above = ModelSettings {
        model: Some("gpt-6.1-sol".into()),
        ..ModelSettings::default()
    };
    assert_eq!(above.over(&model).fallback, model.fallback);
}
