//! Pipeline regression coverage: `apply_markdown_format` (what every
//! Ctrl+B/Ctrl+I keyboard shortcut and toolbar button actually calls) is
//! composed with `compute_spans` (what Editor V2's renderer actually sees),
//! not tested in isolation. A real-machine acceptance pass found Ctrl+B
//! visually producing bold+italic instead of bold-only; these tests prove
//! the formatting-and-rendering pipeline itself is correct end to end, so
//! that specific failure cannot be a pure-parser or pure-formatter bug
//! reappearing unnoticed (its actual cause was GtkSourceView's own,
//! independent syntax highlighting stacking on top of Editor V2's tags -
//! see src/ui.rs's `register_markdown_style_tags`/`highlight_syntax`).

use senatorial_notes::formatting::{FormatAction, apply_markdown_format};
use senatorial_notes::markdown_spans::{Span, SpanKind, compute_spans};

fn find_kind(spans: &[Span], kind: SpanKind) -> Vec<&Span> {
    spans
        .iter()
        .filter(|span| span.kind == kind)
        .collect::<Vec<_>>()
}

fn content<'a>(text: &'a str, span: &Span) -> &'a str {
    &text[span.content_range.clone()]
}

/// One step of the pipeline: apply `action` to the current text/selection,
/// return the new text and the selection `apply_markdown_format` reports
/// (so the next step can act on the same span, exactly like the toolbar/
/// keyboard shortcuts do via the editor's live selection).
fn step(text: &str, start: usize, end: usize, action: FormatAction) -> (String, usize, usize) {
    let edit = apply_markdown_format(text, start, end, action);
    (edit.text, edit.selection_start, edit.selection_end)
}

#[test]
fn ctrl_b_on_plain_selection_produces_bold_only() {
    let source = "Hello world";
    let (text, start, end) = step(source, 6, 11, FormatAction::Bold);
    assert_eq!(text, "Hello **world**");
    let spans = compute_spans(&text);
    assert_eq!(find_kind(&spans, SpanKind::Bold).len(), 1);
    assert_eq!(
        content(&text, find_kind(&spans, SpanKind::Bold)[0]),
        "world"
    );
    assert!(
        find_kind(&spans, SpanKind::Italic).is_empty(),
        "Ctrl+B alone must never produce an Italic span"
    );
    assert_eq!(&text[start..end], "world");
}

#[test]
fn ctrl_i_on_plain_selection_produces_italic_only() {
    let source = "Hello world";
    let (text, ..) = step(source, 6, 11, FormatAction::Italic);
    assert_eq!(text, "Hello *world*");
    let spans = compute_spans(&text);
    assert_eq!(find_kind(&spans, SpanKind::Italic).len(), 1);
    assert!(
        find_kind(&spans, SpanKind::Bold).is_empty(),
        "Ctrl+I alone must never produce a Bold span"
    );
}

#[test]
fn full_toggle_sequence_matches_the_expected_bold_then_bold_italic_then_bold_then_plain() {
    let source = "Hello world";

    // plain -> Ctrl+B -> bold only
    let (text, start, end) = step(source, 6, 11, FormatAction::Bold);
    assert_eq!(text, "Hello **world**");
    {
        let spans = compute_spans(&text);
        assert_eq!(find_kind(&spans, SpanKind::Bold).len(), 1);
        assert!(find_kind(&spans, SpanKind::Italic).is_empty());
    }

    // Ctrl+I on that -> bold + italic
    let (text, start, end) = step(&text, start, end, FormatAction::Italic);
    let spans = compute_spans(&text);
    let bold = find_kind(&spans, SpanKind::Bold);
    let italic = find_kind(&spans, SpanKind::Italic);
    assert_eq!(bold.len(), 1, "text: {text:?}");
    assert_eq!(italic.len(), 1, "text: {text:?}");
    assert_eq!(content(&text, bold[0]), "world");
    assert_eq!(content(&text, italic[0]), "world");

    // Ctrl+I again -> bold only
    let (text, start, end) = step(&text, start, end, FormatAction::Italic);
    {
        let spans = compute_spans(&text);
        assert_eq!(find_kind(&spans, SpanKind::Bold).len(), 1, "text: {text:?}");
        assert!(
            find_kind(&spans, SpanKind::Italic).is_empty(),
            "toggling italic off must not leave a stray Italic span; text: {text:?}"
        );
    }

    // Ctrl+B again -> plain
    let (text, ..) = step(&text, start, end, FormatAction::Bold);
    assert_eq!(
        text, "Hello world",
        "must return to exactly the original plain text"
    );
    let spans = compute_spans(&text);
    assert!(find_kind(&spans, SpanKind::Bold).is_empty());
    assert!(find_kind(&spans, SpanKind::Italic).is_empty());
}

#[test]
fn toggling_bold_off_independently_of_italic() {
    // Start from bold+italic, remove only bold, keep italic.
    let source = "Hello ***world*** end";
    let start = source.find("world").unwrap();
    let end = start + "world".len();
    let (text, start, end) = step(source, start, end, FormatAction::Bold);
    let spans = compute_spans(&text);
    assert!(
        find_kind(&spans, SpanKind::Bold).is_empty(),
        "text: {text:?}"
    );
    assert_eq!(
        find_kind(&spans, SpanKind::Italic).len(),
        1,
        "text: {text:?}"
    );
    assert_eq!(&text[start..end], "world");
}

#[test]
fn toggling_italic_off_independently_of_bold() {
    let source = "Hello ***world*** end";
    let start = source.find("world").unwrap();
    let end = start + "world".len();
    let (text, ..) = step(source, start, end, FormatAction::Italic);
    let spans = compute_spans(&text);
    assert_eq!(find_kind(&spans, SpanKind::Bold).len(), 1, "text: {text:?}");
    assert!(
        find_kind(&spans, SpanKind::Italic).is_empty(),
        "text: {text:?}"
    );
}

#[test]
fn cursor_without_selection_inserts_a_wrapped_placeholder() {
    let source = "Hello  world"; // two spaces: an empty-ish insertion point
    let cursor = 6;
    let (text, start, end) = step(source, cursor, cursor, FormatAction::Bold);
    assert!(text.contains("**bold text**"), "text: {text:?}");
    let spans = compute_spans(&text);
    assert_eq!(find_kind(&spans, SpanKind::Bold).len(), 1);
    assert_eq!(&text[start..end], "bold text");
}

#[test]
fn selection_at_the_very_start_and_end_of_the_buffer_does_not_panic_and_formats_correctly() {
    let source = "word";
    let (text, ..) = step(source, 0, 4, FormatAction::Bold); // whole buffer selected
    assert_eq!(text, "**word**");
    let spans = compute_spans(&text);
    assert_eq!(find_kind(&spans, SpanKind::Bold).len(), 1);

    let empty = "";
    let (text, ..) = step(empty, 0, 0, FormatAction::Italic); // empty buffer, cursor at 0
    assert!(text.contains("*italic text*"));
    let _ = compute_spans(&text); // must not panic
}

#[test]
fn rapid_repeated_bold_toggles_never_accumulate_markers() {
    let mut text = "Hello world".to_string();
    let mut start = 6usize;
    let mut end = 11usize;
    for i in 0..40 {
        let (next_text, next_start, next_end) = step(&text, start, end, FormatAction::Bold);
        text = next_text;
        start = next_start;
        end = next_end;
        let spans = compute_spans(&text);
        let bold_count = find_kind(&spans, SpanKind::Bold).len();
        let expected = if i % 2 == 0 { 1 } else { 0 };
        assert_eq!(
            bold_count,
            expected,
            "after {} toggles, text was {text:?}",
            i + 1
        );
        // Never more than exactly one pair of ** on each side, however many
        // times the same toggle has been pressed.
        assert!(!text.contains("****"), "markers must never stack: {text:?}");
    }
    assert_eq!(
        text, "Hello world",
        "must land back on plain text after an even number of toggles"
    );
}

#[test]
fn rapid_alternating_bold_and_italic_toggles_never_produce_stray_markers() {
    let mut text = "Hello world".to_string();
    let mut start = 6usize;
    let mut end = 11usize;
    for i in 0..30 {
        let action = if i % 2 == 0 {
            FormatAction::Bold
        } else {
            FormatAction::Italic
        };
        let (next_text, next_start, next_end) = step(&text, start, end, action);
        text = next_text;
        start = next_start;
        end = next_end;
        let _ = compute_spans(&text); // must not panic at any intermediate state
        assert!(!text.contains("****"), "text: {text:?}");
    }
}
