//! The Speaker's office as a server starts it: who the Speaker is, read
//! from configuration (with the older forms it still accepts), and the
//! built-in session beside it.
//!
//! - A Speaker named with a Senate path (`/acme::senator::wendell`) is a
//!   senator of the parent: it must be the configured `parent`, else the
//!   server does not start. Whether it is a person there is the parent's to
//!   check, when this Senate registers with it.
//! - A bare name (`speaker = "wendell"`) is read for compatibility: the
//!   parent's senator when a parent is configured, else this Senate's own.
//! - No Speaker named and the built-in session running: the built-in
//!   session is the Speaker, an unattended root, as a Senate configured
//!   before Speakers were named had it.
//!
//! Each reading of an older form leaves a [`Note`]: logged once (the store's
//! registry remembers it was), and kept on the hub for whoever shows the
//! Senate's status.

use senate_core::{Locale, Roster, RosterConfig, SenatePath, SenatorId, SenatorName, SpeakerName};

use crate::config::ConfigError;
use crate::seats::rebuild;

/// The name of the built-in session when configuration gives none.
pub const DEFAULT_BUILT_IN_NAME: &str = "secretary";

/// The built-in session's default name.
pub fn default_built_in_name() -> SenatorName {
    DEFAULT_BUILT_IN_NAME
        .parse()
        .unwrap_or_else(|_| unreachable!("a valid flat name, not a keyword"))
}

/// How loudly a [`Note`] is logged the first time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    /// A correct configuration, said in plain words.
    Info,
    /// A configuration read from an older form, or assumed: worth fixing.
    Warn,
}

/// Something a start assumed or settled, to say once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Note {
    /// What the registry remembers it by.
    pub key: String,
    pub level: Level,
    pub text: String,
}

/// The built-in session is the Speaker: nothing names one.
fn built_in_is_speaker(locale: Locale, built_in: &SenatorName) -> String {
    let id = SenatorId::here(built_in.clone()).in_text();
    match locale {
        Locale::Zh => format!(
            "配置未指定 Speaker：内置会话 {id} 担任本 Senate 的 Speaker（无人值守的根 Senate）。如需由某人担任，请设置 speaker = \"senator::<名字>\"。"
        ),
        Locale::En => format!(
            "No Speaker is configured: the built-in session {id} is this Senate's Speaker (an unattended root). To have a person preside, set speaker = \"senator::<name>\"."
        ),
    }
}

/// A bare Speaker name read as the parent's senator.
fn bare_speaker_of_parent(locale: Locale, bare: &SenatorName, id: &SenatorId) -> String {
    let id = id.in_text();
    match locale {
        Locale::Zh => format!(
            "speaker = \"{bare}\" 未写明所属 Senate；本 Senate 配置了上级，因此按上级 Senate 的 senator {id} 理解。请改写为 speaker = \"{}\"。",
            id.trim_matches(['[', ']'])
        ),
        Locale::En => format!(
            "speaker = \"{bare}\" names no Senate; this Senate has a parent, so it is read as the parent's senator {id}. Write speaker = \"{}\".",
            id.trim_matches(['[', ']'])
        ),
    }
}

/// A bare Speaker name read as this Senate's senator.
fn bare_speaker_here(locale: Locale, bare: &SenatorName) -> String {
    let id = SenatorId::here(bare.clone());
    match locale {
        Locale::Zh => format!(
            "speaker = \"{bare}\" 未写明所属 Senate；本 Senate 没有上级，因此按本 Senate 的 senator {} 理解。请改写为 speaker = \"{id}\"。",
            id.in_text()
        ),
        Locale::En => format!(
            "speaker = \"{bare}\" names no Senate; this Senate has no parent, so it is read as this Senate's senator {}. Write speaker = \"{id}\".",
            id.in_text()
        ),
    }
}

/// A child's Speaker, a person of the parent: correct as written.
fn speaker_of_parent(locale: Locale, id: &SenatorId) -> String {
    let id = id.in_text();
    match locale {
        Locale::Zh => format!(
            "本 Senate 的 Speaker 是上级 Senate 的成员 {id}。上级会在本 Senate 向它注册时确认这是一个人（而不是 agent）。"
        ),
        Locale::En => format!(
            "This Senate's Speaker is {id}, a person of the parent Senate. The parent confirms that it is a person (not an agent) when this Senate registers with it."
        ),
    }
}

/// Read the configured Speaker against the parent, then seat the built-in
/// session and, when nothing names a Speaker, make it the Speaker. The
/// notes say what was assumed.
pub(crate) fn settle(
    mut config: RosterConfig,
    parent: Option<&SenatePath>,
    built_in: Option<SenatorName>,
    locale: Locale,
) -> Result<(Roster, Vec<Note>), ConfigError> {
    let mut notes = Vec::new();
    let mut speaker = config.speaker.take();
    if let Some(named) = &speaker
        && named.bare
    {
        let bare = named.id.name().clone();
        match parent {
            Some(parent) => {
                let id = SenatorId::of(parent.clone(), bare.clone());
                notes.push(Note {
                    key: format!("speaker-bare-parent:{bare}"),
                    level: Level::Warn,
                    text: bare_speaker_of_parent(locale, &bare, &id),
                });
                speaker = Some(SpeakerName::id(id));
            }
            None => notes.push(Note {
                key: format!("speaker-bare-here:{bare}"),
                level: Level::Warn,
                text: bare_speaker_here(locale, &bare),
            }),
        }
    }
    if let (Some(named), Some(parent)) = (&speaker, parent)
        && named.id.senate().is_none()
    {
        return Err(ConfigError::Invalid(format!(
            "the Speaker {} is a senator of this Senate, but this Senate has a parent ({}): a child's Speaker is a senator of its parent, written {}::senator::<name>",
            named.id,
            parent.clone().absolute(),
            parent.clone().absolute()
        )));
    }
    if let Some(named) = &speaker
        && let Some(senate) = named.id.senate()
    {
        match parent {
            Some(parent) if parent.same_names(senate) => notes.push(Note {
                key: format!("speaker-of-parent:{}", named.id),
                level: Level::Info,
                text: speaker_of_parent(locale, &named.id),
            }),
            Some(parent) => {
                return Err(ConfigError::Invalid(format!(
                    "the Speaker {} is a senator of {senate}, but this Senate's parent is {}: a child's Speaker is a senator of its parent",
                    named.id,
                    parent.clone().absolute()
                )));
            }
            None => {
                return Err(ConfigError::Invalid(format!(
                    "the Speaker {} is a senator of {senate}, but this Senate names no parent: set parent = \"{}\", or name one of this Senate's senators (senator::<name>)",
                    named.id,
                    senate
                        .names()
                        .iter()
                        .map(|n| n.as_str())
                        .collect::<Vec<_>>()
                        .join("/")
                )));
            }
        }
    }
    if speaker.is_none()
        && let Some(built_in) = &built_in
    {
        notes.push(Note {
            key: format!("built-in-speaker:{built_in}"),
            level: Level::Warn,
            text: built_in_is_speaker(locale, built_in),
        });
        speaker = Some(SpeakerName::id(SenatorId::here(built_in.clone())));
    }
    config.speaker = speaker;
    let roster =
        rebuild(config, built_in).map_err(|e| ConfigError::Invalid(format!("the Speaker: {e}")))?;
    Ok((roster, notes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use senate_core::{Name, Principal, SenatorKind};

    fn alice() -> RosterConfig {
        Roster::of_person(Name::new("alice").unwrap()).to_config()
    }

    #[test]
    fn every_note_reads_in_both_languages_with_ids_bracketed() {
        let name: SenatorName = "wendell".parse().unwrap();
        let id = SenatorId::of("acme".parse().unwrap(), name.clone());
        for locale in Locale::ALL {
            for text in [
                built_in_is_speaker(locale, &default_built_in_name()),
                bare_speaker_of_parent(locale, &name, &id),
                bare_speaker_here(locale, &name),
                speaker_of_parent(locale, &id),
            ] {
                assert!(
                    text.contains("Senate") || text.contains("Speaker"),
                    "{text}"
                );
                assert!(text.contains('['), "ids are bracketed: {text}");
            }
        }
        assert_ne!(
            bare_speaker_here(Locale::Zh, &name),
            bare_speaker_here(Locale::En, &name)
        );
        assert!(built_in_is_speaker(Locale::Zh, &default_built_in_name()).contains("内置会话"));
    }

    #[test]
    fn a_person_of_the_parent_as_speaker_is_said_at_info_and_in_plain_words() {
        let parent: SenatePath = "acme".parse().unwrap();
        let mut config = alice();
        config.speaker = Some(SpeakerName::id(SenatorId::of(
            parent.clone(),
            "wendell".parse().unwrap(),
        )));
        let (_, notes) = settle(config, Some(&parent), None, Locale::En).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].level, Level::Info);
        assert!(!notes[0].text.contains("must"), "{}", notes[0].text);
        assert!(
            !notes[0].text.contains("not supported"),
            "{}",
            notes[0].text
        );
        // Older forms and assumptions stay warnings.
        let mut bare = alice();
        bare.speaker = Some(SpeakerName::from("wendell".parse::<SenatorName>().unwrap()));
        let (_, notes) = settle(bare, Some(&parent), None, Locale::En).unwrap();
        assert_eq!(notes[0].level, Level::Warn);
        let (_, notes) = settle(alice(), None, Some(default_built_in_name()), Locale::En).unwrap();
        assert_eq!(notes[0].level, Level::Warn);
    }

    #[test]
    fn the_built_in_session_is_the_speaker_when_nothing_names_one() {
        let (roster, notes) =
            settle(alice(), None, Some(default_built_in_name()), Locale::En).unwrap();
        let office = roster.office().unwrap();
        assert_eq!(office.kind, SenatorKind::BuiltIn);
        assert!(office.triages());
        assert_eq!(notes.len(), 1);
        // Several principals: the built-in session sits under nobody.
        let several = Roster::new(
            vec![
                Principal::new(Name::new("alice").unwrap()),
                Principal::new(Name::new("bob").unwrap()),
            ],
            vec![],
        )
        .unwrap()
        .to_config();
        let (roster, _) = settle(several, None, Some(default_built_in_name()), Locale::En).unwrap();
        assert_eq!(roster.office().unwrap().kind, SenatorKind::BuiltIn);
    }
}
