use senatorial_notes::formatting::{FormatAction, FormatEdit, apply_markdown_format};

fn selected(edit: &FormatEdit) -> &str {
    &edit.text[edit.selection_start..edit.selection_end]
}

#[test]
fn inline_formatting_wraps_selection_without_losing_text() {
    let edit = apply_markdown_format("hello world", 6, 11, FormatAction::Bold);
    assert_eq!(edit.text, "hello **world**");
    assert_eq!(
        &edit.text[edit.selection_start..edit.selection_end],
        "world"
    );
}

#[test]
fn heading_replaces_existing_heading_level() {
    let edit = apply_markdown_format("## Existing heading\nBody", 5, 5, FormatAction::Heading1);
    assert_eq!(edit.text, "# Existing heading\nBody");
}

#[test]
fn checklist_formats_each_selected_line() {
    let edit = apply_markdown_format("first\nsecond", 0, 12, FormatAction::Checklist);
    assert_eq!(edit.text, "- [ ] first\n- [ ] second");
}

#[test]
fn bold_toggles_on_then_off_for_the_same_selection() {
    let on = apply_markdown_format("word", 0, 4, FormatAction::Bold);
    assert_eq!(on.text, "**word**");
    assert_eq!(selected(&on), "word");

    // Pressing Bold again with the wrapped word selected removes the markers.
    let off = apply_markdown_format(
        &on.text,
        on.selection_start,
        on.selection_end,
        FormatAction::Bold,
    );
    assert_eq!(off.text, "word");
    assert_eq!(selected(&off), "word");
}

#[test]
fn italic_toggles_on_then_off_and_does_not_stack() {
    let on = apply_markdown_format("word", 0, 4, FormatAction::Italic);
    assert_eq!(on.text, "*word*");

    let off = apply_markdown_format(
        &on.text,
        on.selection_start,
        on.selection_end,
        FormatAction::Italic,
    );
    assert_eq!(off.text, "word");
    assert_eq!(selected(&off), "word");
}

#[test]
fn strikethrough_and_highlight_toggle_off_cleanly() {
    for action in [FormatAction::Strikethrough, FormatAction::Highlight] {
        let on = apply_markdown_format("word", 0, 4, action);
        let off = apply_markdown_format(&on.text, on.selection_start, on.selection_end, action);
        assert_eq!(
            off.text, "word",
            "{action:?} should toggle back to plain text"
        );
    }
}

#[test]
fn selecting_the_markers_too_still_toggles_off() {
    // The whole `**word**` run is selected, markers included.
    let off = apply_markdown_format("a **word** b", 2, 10, FormatAction::Bold);
    assert_eq!(off.text, "a word b");
    assert_eq!(selected(&off), "word");
}

#[test]
fn italic_on_bold_text_nests_instead_of_rewriting_the_bold() {
    // Inner `word` of `**word**` is selected; Italic must not strip a `*`.
    let edit = apply_markdown_format("**word**", 2, 6, FormatAction::Italic);
    assert_eq!(edit.text, "***word***");
    assert_eq!(selected(&edit), "word");

    // And Bold on that same selection removes only the bold pair.
    let back = apply_markdown_format("***word***", 3, 7, FormatAction::Bold);
    assert_eq!(back.text, "*word*");
    assert_eq!(selected(&back), "word");
}

#[test]
fn italic_removes_the_outer_italic_from_bold_italic_text() {
    let edit = apply_markdown_format("***word***", 3, 7, FormatAction::Italic);
    assert_eq!(edit.text, "**word**");
    assert_eq!(selected(&edit), "word");
}

#[test]
fn bold_toggle_off_with_the_cursor_inside_the_span() {
    // No selection: caret sits between `wo` and `rd` inside `**word**`.
    let edit = apply_markdown_format("**word**", 4, 4, FormatAction::Bold);
    assert_eq!(edit.text, "word");
}

#[test]
fn heading_toggles_back_to_a_paragraph_on_a_second_press() {
    let heading = apply_markdown_format("Title\nbody", 0, 0, FormatAction::Heading1);
    assert_eq!(heading.text, "# Title\nbody");

    let paragraph = apply_markdown_format(&heading.text, 0, 0, FormatAction::Heading1);
    assert_eq!(paragraph.text, "Title\nbody");
}

#[test]
fn heading_never_stacks_hash_prefixes() {
    let mut text = String::from("Note");
    for _ in 0..5 {
        let edit = apply_markdown_format(&text, 0, 0, FormatAction::Heading2);
        text = edit.text;
        // Only ever `## Note` or `Note`, never `## ## Note`.
        assert!(
            text == "## Note" || text == "Note",
            "stacked heading markers: {text:?}"
        );
    }
}
