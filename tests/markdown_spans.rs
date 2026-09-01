//! Regression coverage for the pure Markdown style-span parser used by
//! Editor V2's live-preview rendering. Pure `&str` -> `Vec<Span>` tests, no
//! GTK required - matches `tests/formatting.rs`'s convention.

use senatorial_notes::markdown_spans::{Span, SpanKind, compute_spans};

fn content<'a>(text: &'a str, span: &Span) -> &'a str {
    &text[span.content_range.clone()]
}

fn find_kind(spans: &[Span], kind: SpanKind) -> Vec<&Span> {
    spans
        .iter()
        .filter(|span| span.kind == kind)
        .collect::<Vec<_>>()
}

#[test]
fn heading_levels_are_detected_with_correct_content() {
    let text = "# One\n## Two\n### Three\nplain paragraph";
    let spans = compute_spans(text);
    let h1 = find_kind(&spans, SpanKind::Heading1);
    let h2 = find_kind(&spans, SpanKind::Heading2);
    let h3 = find_kind(&spans, SpanKind::Heading3);
    assert_eq!(h1.len(), 1);
    assert_eq!(content(text, h1[0]), "One");
    assert_eq!(h2.len(), 1);
    assert_eq!(content(text, h2[0]), "Two");
    assert_eq!(h3.len(), 1);
    assert_eq!(content(text, h3[0]), "Three");
    // A fourth-level "####" is not one of the three supported levels and
    // must not be misparsed as anything.
    let unsupported = compute_spans("#### Four");
    assert!(
        unsupported.iter().all(|span| !matches!(
            span.kind,
            SpanKind::Heading1 | SpanKind::Heading2 | SpanKind::Heading3
        )),
        "unsupported heading depth must not produce a heading span"
    );
}

#[test]
fn bold_is_detected() {
    let text = "plain **bold** plain";
    let spans = compute_spans(text);
    let bold = find_kind(&spans, SpanKind::Bold);
    assert_eq!(bold.len(), 1);
    assert_eq!(content(text, bold[0]), "bold");
}

#[test]
fn italic_is_detected() {
    let text = "plain *italic* plain";
    let spans = compute_spans(text);
    let italic = find_kind(&spans, SpanKind::Italic);
    assert_eq!(italic.len(), 1);
    assert_eq!(content(text, italic[0]), "italic");
}

#[test]
fn triple_star_produces_coexisting_bold_and_italic_over_the_same_content() {
    let text = "plain ***both*** plain";
    let spans = compute_spans(text);
    let bold = find_kind(&spans, SpanKind::Bold);
    let italic = find_kind(&spans, SpanKind::Italic);
    assert_eq!(bold.len(), 1);
    assert_eq!(italic.len(), 1);
    assert_eq!(content(text, bold[0]), "both");
    assert_eq!(content(text, italic[0]), "both");
    assert_eq!(
        bold[0].content_range, italic[0].content_range,
        "bold and italic must cover the exact same content range so they visually combine"
    );
}

#[test]
fn bold_containing_nested_italic_matches_the_toolbar_toggle_sequence() {
    // plain -> Bold -> Bold+Italic -> Bold -> plain
    let text = "plain **bold *both* bold** plain";
    let spans = compute_spans(text);
    let bold = find_kind(&spans, SpanKind::Bold);
    let italic = find_kind(&spans, SpanKind::Italic);
    assert_eq!(bold.len(), 1);
    assert_eq!(content(text, bold[0]), "bold *both* bold");
    assert_eq!(italic.len(), 1);
    assert_eq!(content(text, italic[0]), "both");
}

#[test]
fn italic_containing_nested_bold_is_also_supported() {
    let text = "plain *italic **both** italic* plain";
    let spans = compute_spans(text);
    let bold = find_kind(&spans, SpanKind::Bold);
    let italic = find_kind(&spans, SpanKind::Italic);
    assert_eq!(italic.len(), 1);
    assert_eq!(content(text, italic[0]), "italic **both** italic");
    assert_eq!(bold.len(), 1);
    assert_eq!(content(text, bold[0]), "both");
}

#[test]
fn strikethrough_and_highlight_are_detected() {
    let text = "a ~~struck~~ b ==marked== c";
    let spans = compute_spans(text);
    let strike = find_kind(&spans, SpanKind::Strikethrough);
    let highlight = find_kind(&spans, SpanKind::Highlight);
    assert_eq!(strike.len(), 1);
    assert_eq!(content(text, strike[0]), "struck");
    assert_eq!(highlight.len(), 1);
    assert_eq!(content(text, highlight[0]), "marked");
}

#[test]
fn inline_code_suppresses_formatting_markers_inside_it() {
    let text = "before `**not bold**` after";
    let spans = compute_spans(text);
    let code = find_kind(&spans, SpanKind::InlineCode);
    assert_eq!(code.len(), 1);
    assert_eq!(content(text, code[0]), "**not bold**");
    assert!(
        find_kind(&spans, SpanKind::Bold).is_empty(),
        "markers inside inline code must never produce a Bold span"
    );
}

#[test]
fn escaped_markers_are_not_interpreted() {
    let text = r"plain \*\*not bold\*\* plain";
    let spans = compute_spans(text);
    assert!(
        find_kind(&spans, SpanKind::Bold).is_empty(),
        "escaped ** must never be treated as a bold marker"
    );
}

#[test]
fn unmatched_markers_produce_no_span_and_are_left_untouched() {
    let text = "plain **unclosed and *also unclosed";
    let spans = compute_spans(text); // must not panic
    assert!(find_kind(&spans, SpanKind::Bold).is_empty());
    assert!(find_kind(&spans, SpanKind::Italic).is_empty());
}

#[test]
fn links_separate_display_text_from_the_url_marker() {
    let text = "see [the docs](https://example.com/path) here";
    let spans = compute_spans(text);
    let links = find_kind(&spans, SpanKind::Link);
    assert_eq!(links.len(), 1);
    assert_eq!(content(text, links[0]), "the docs");
}

#[test]
fn a_star_inside_link_text_does_not_confuse_emphasis_scanning() {
    let text = "[a * b](https://example.com)";
    let spans = compute_spans(text); // must not panic
    assert!(
        find_kind(&spans, SpanKind::Italic).is_empty(),
        "a lone * inside link text must not pair with anything outside the link"
    );
}

#[test]
fn lists_and_checklists_are_detected() {
    let bullet = compute_spans("- an item");
    assert_eq!(find_kind(&bullet, SpanKind::BulletItem).len(), 1);

    let star_bullet = compute_spans("* an item");
    assert_eq!(find_kind(&star_bullet, SpanKind::BulletItem).len(), 1);

    let numbered_text = "1. an item";
    let numbered = compute_spans(numbered_text);
    let numbered_items = find_kind(&numbered, SpanKind::NumberedItem);
    assert_eq!(numbered_items.len(), 1);
    assert_eq!(content(numbered_text, numbered_items[0]), "an item");

    let unchecked_text = "- [ ] todo";
    let unchecked = compute_spans(unchecked_text);
    assert_eq!(
        find_kind(&unchecked, SpanKind::ChecklistItem { checked: false }).len(),
        1
    );

    let checked_text = "- [x] done";
    let checked = compute_spans(checked_text);
    assert_eq!(
        find_kind(&checked, SpanKind::ChecklistItem { checked: true }).len(),
        1
    );
}

#[test]
fn quote_and_divider_are_detected() {
    let quote_text = "> a quoted line";
    let quote = compute_spans(quote_text);
    let quotes = find_kind(&quote, SpanKind::Quote);
    assert_eq!(quotes.len(), 1);
    assert_eq!(content(quote_text, quotes[0]), "a quoted line");

    let divider = compute_spans("above\n---\nbelow");
    assert_eq!(find_kind(&divider, SpanKind::Divider).len(), 1);
}

#[test]
fn unrecognised_syntax_produces_no_spans_and_is_never_rewritten() {
    // A Markdown table, which this parser deliberately does not support -
    // must not panic, and must not produce any span that would restyle it.
    let text = "| a | b |\n| - | - |\n| 1 | 2 |";
    let spans = compute_spans(text);
    assert!(
        spans.is_empty(),
        "unsupported syntax must produce zero spans, never a guess"
    );
}

#[test]
fn unicode_content_round_trips_through_span_ranges() {
    let text = "Привет **мир** café **naïve** 👍🏽 plain **bold** end";
    let spans = compute_spans(text); // must not panic on multi-byte content
    let bold = find_kind(&spans, SpanKind::Bold);
    let bold_contents: Vec<&str> = bold.iter().map(|span| content(text, span)).collect();
    assert!(bold_contents.contains(&"мир"));
    assert!(bold_contents.contains(&"naïve"));
    assert!(bold_contents.contains(&"bold"));
}

#[test]
fn unicode_immediately_adjacent_to_and_inside_delimiters_does_not_panic_or_misplace_content() {
    for text in [
        "**Привет**",
        "*café*",
        "~~👍🏽~~",
        "==naïve==",
        "**café *naïve* café**",
        "# Заголовок",
        "- Список",
        "> Цитата",
    ] {
        let spans = compute_spans(text);
        for span in &spans {
            // Every range must land on a valid UTF-8 char boundary - this
            // would already panic on slicing if not, so reaching this
            // assertion at all is part of the proof.
            let _ = &text[span.content_range.clone()];
            for marker in &span.marker_ranges {
                let _ = &text[marker.clone()];
            }
        }
    }
}

#[test]
fn adversarial_input_never_panics() {
    let cases = [
        "",
        "*",
        "**",
        "***",
        "****",
        &"*".repeat(500),
        "\\",
        "\\*",
        "[",
        "[]",
        "[]()",
        "`",
        "``",
        "~~",
        "==",
        "#",
        "# ",
        "- [ ",
        "- [x",
        "1.",
        "1. ",
        "\n\n\n",
        "a\r\nb",
    ];
    for case in cases {
        let _ = compute_spans(case);
    }
}

#[test]
fn realistic_large_document_recomputes_quickly() {
    // A ~250 KB realistic note (many paragraphs, ordinary formatting
    // density - not adversarial repeat runs, which the panic sweep above
    // already covers separately) must stay comfortably fast, since this
    // runs on every debounced keystroke pause in the real editor.
    let paragraph = "# Section heading\n\n\
        This paragraph has **bold text**, *italic text*, ***both***, a \
        **bold span with *nested italic* inside it**, some ~~struck~~ and \
        ==highlighted== text, `inline code`, and a [link](https://example.com/page).\n\n\
        > A block quote for good measure.\n\n\
        - A bullet item\n\
        - Another bullet with **bold** in it\n\
        1. A numbered item\n\
        2. Another numbered item\n\
        - [ ] An open task\n\
        - [x] A finished task\n\n\
        ---\n\n";
    let large_document = paragraph.repeat(600); // roughly 240 KB
    assert!(large_document.len() > 200_000);

    let start = std::time::Instant::now();
    let spans = compute_spans(&large_document);
    let elapsed = start.elapsed();

    assert!(!spans.is_empty());
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "compute_spans on a ~250 KB realistic document took {elapsed:?}, expected well under \
         500ms since it runs on every debounced keystroke pause in the real editor"
    );
}
