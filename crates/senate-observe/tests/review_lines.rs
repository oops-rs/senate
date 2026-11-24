//! Security regression tests for the published-preview normalization.

use senate_observe::first_line;

#[test]
fn line_is_bounded_to_80_scalars_and_320_bytes() {
    let line = first_line(&"\u{10FFFF}".repeat(500)).unwrap_or_default();
    assert!(line.chars().count() <= 80 && line.len() <= 320);
}

#[test]
fn invisible_format_characters_are_stripped() {
    // U+E0041.. are Unicode TAG characters (invisible "ASCII smuggling").
    let hidden: String = "secret"
        .chars()
        .filter_map(|c| char::from_u32(0xE0000 + c as u32))
        .collect();
    let line = first_line(&format!("ok{hidden}\u{200B}\u{FEFF}")).unwrap_or_default();
    assert_eq!(line, "ok", "got {:?}", line);
}

#[test]
fn only_the_first_visual_line_is_exported() {
    assert_eq!(first_line("title\u{2028}second").as_deref(), Some("title"));
    assert_eq!(first_line("title\rsecond").as_deref(), Some("title"));
}
