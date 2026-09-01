//! Pure Markdown style-span detection for Editor V2's live-preview
//! rendering.
//!
//! This module never touches storage and never rewrites text. It only
//! answers "given this literal Markdown text, which byte ranges should be
//! rendered bold/italic/etc, and which byte ranges are marker punctuation
//! that should be visually subdued?" The `GtkSourceView` buffer always holds
//! the literal Markdown; this is a read-only analysis pass over it, and the
//! caller (`src/ui.rs`) turns the result into purely presentational
//! `GtkTextTag`s. Anything this parser does not recognise is left with no
//! span at all, so it is never restyled, hidden, or rewritten.
//!
//! Offsets are always **byte offsets** into the input `&str`, matching Rust
//! string convention and `formatting.rs`. Converting to GTK's character
//! offsets happens exactly once, at tag-application time, in `ui.rs`.

// `marker_ranges` is deliberately always `Vec<Range<usize>>`, including for
// constructs (headings, quotes, list items) that only ever have one marker
// range - clippy's suggested fix here (collecting the range's values into a
// `Vec<usize>`) does not apply to this data model.
#![allow(clippy::single_range_in_vec_init)]

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    Heading1,
    Heading2,
    Heading3,
    Bold,
    Italic,
    Strikethrough,
    Highlight,
    InlineCode,
    Quote,
    BulletItem,
    NumberedItem,
    ChecklistItem { checked: bool },
    Link,
    Divider,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub kind: SpanKind,
    /// Marker punctuation for this span (e.g. the two `*` pairs of
    /// `**bold**`, or the `- [ ] ` prefix of a checklist item). Rendered
    /// visually subdued - muted colour only, never a different size, so a
    /// line's geometry never changes because of styling.
    pub marker_ranges: Vec<Range<usize>>,
    /// The semantic content this span's visual style applies to. Empty for
    /// a divider, which has no content of its own.
    pub content_range: Range<usize>,
}

/// Computes every style span in `text`. Infallible: never panics, on any
/// input, including empty strings, unmatched/malformed markers, and
/// adversarial repeat runs - unpaired or unrecognised syntax simply
/// produces no span for that text, left exactly as typed.
pub fn compute_spans(text: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut line_start = 0;
    for line in text.split('\n') {
        scan_line(text, line_start..line_start + line.len(), &mut spans);
        line_start += line.len() + 1;
    }
    spans
}

fn scan_line(text: &str, line: Range<usize>, spans: &mut Vec<Span>) {
    let line_text = &text[line.clone()];
    let trimmed_start = line_text.len() - line_text.trim_start().len();
    let content_start = line.start + trimmed_start;

    if is_divider(line_text.trim()) {
        spans.push(Span {
            kind: SpanKind::Divider,
            marker_ranges: vec![line.clone()],
            content_range: line.end..line.end,
        });
        return;
    }

    if let Some((level, prefix_end)) = heading_prefix(text, content_start, line.end) {
        let kind = match level {
            1 => SpanKind::Heading1,
            2 => SpanKind::Heading2,
            _ => SpanKind::Heading3,
        };
        spans.push(Span {
            kind,
            marker_ranges: vec![line.start..prefix_end],
            content_range: prefix_end..line.end,
        });
        scan_inline(text, prefix_end..line.end, spans);
        return;
    }

    if let Some(prefix_end) = literal_prefix(text, content_start, line.end, "> ") {
        spans.push(Span {
            kind: SpanKind::Quote,
            marker_ranges: vec![line.start..prefix_end],
            content_range: prefix_end..line.end,
        });
        scan_inline(text, prefix_end..line.end, spans);
        return;
    }

    if let Some(checked) = checklist_marker(text, content_start, line.end) {
        let prefix_end = content_start + checklist_prefix_len(checked);
        spans.push(Span {
            kind: SpanKind::ChecklistItem { checked },
            marker_ranges: vec![line.start..prefix_end],
            content_range: prefix_end..line.end,
        });
        scan_inline(text, prefix_end..line.end, spans);
        return;
    }

    if let Some(prefix_end) = literal_prefix(text, content_start, line.end, "- ")
        .or_else(|| literal_prefix(text, content_start, line.end, "* "))
    {
        spans.push(Span {
            kind: SpanKind::BulletItem,
            marker_ranges: vec![line.start..prefix_end],
            content_range: prefix_end..line.end,
        });
        scan_inline(text, prefix_end..line.end, spans);
        return;
    }

    if let Some(prefix_end) = numbered_prefix(text, content_start, line.end) {
        spans.push(Span {
            kind: SpanKind::NumberedItem,
            marker_ranges: vec![line.start..prefix_end],
            content_range: prefix_end..line.end,
        });
        scan_inline(text, prefix_end..line.end, spans);
        return;
    }

    scan_inline(text, line, spans);
}

fn is_divider(trimmed: &str) -> bool {
    trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-')
}

fn heading_prefix(text: &str, start: usize, line_end: usize) -> Option<(u8, usize)> {
    let slice = &text[start..line_end];
    let hashes = slice.chars().take_while(|c| *c == '#').count();
    if !(1..=3).contains(&hashes) {
        return None;
    }
    let rest = &slice[hashes..];
    if !rest.starts_with(' ') {
        return None;
    }
    Some((hashes as u8, start + hashes + 1))
}

fn literal_prefix(text: &str, start: usize, line_end: usize, prefix: &str) -> Option<usize> {
    let slice = &text[start..line_end];
    slice.starts_with(prefix).then_some(start + prefix.len())
}

fn checklist_prefix_len(checked: bool) -> usize {
    if checked {
        "- [x] ".len()
    } else {
        "- [ ] ".len()
    }
}

fn checklist_marker(text: &str, start: usize, line_end: usize) -> Option<bool> {
    let slice = &text[start..line_end];
    if slice.starts_with("- [ ] ") {
        Some(false)
    } else if slice.starts_with("- [x] ") || slice.starts_with("- [X] ") {
        Some(true)
    } else {
        None
    }
}

fn numbered_prefix(text: &str, start: usize, line_end: usize) -> Option<usize> {
    let slice = &text[start..line_end];
    let digits = slice.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &slice[digits..];
    if rest.starts_with(". ") {
        Some(start + digits + 2)
    } else {
        None
    }
}

/// Scans a line's content (after any block-level prefix has already been
/// stripped and handled) for inline constructs: inline code first (highest
/// precedence - it suppresses every other marker inside it), then links,
/// then emphasis (bold/italic/bold+italic, with one level of nesting either
/// way), then strikethrough and highlight.
fn scan_inline(text: &str, range: Range<usize>, spans: &mut Vec<Span>) {
    let code_ranges = find_delimited(text, range.clone(), "`", "`", |_| true);
    for code in &code_ranges {
        spans.push(Span {
            kind: SpanKind::InlineCode,
            marker_ranges: vec![
                code.start..code.start + 1,
                code.end.saturating_sub(1)..code.end,
            ],
            content_range: code.start + 1..code.end.saturating_sub(1),
        });
    }

    let masked = code_ranges;
    scan_links(text, range.clone(), &masked, spans);
    let link_ranges: Vec<Range<usize>> = spans
        .iter()
        .filter(|span| span.kind == SpanKind::Link && range.contains(&span.content_range.start))
        .map(full_link_range)
        .collect();
    let mut excluded = masked;
    excluded.extend(link_ranges);
    excluded.sort_by_key(|r| r.start);

    scan_emphasis(text, range, &excluded, spans);
}

fn full_link_range(span: &Span) -> Range<usize> {
    let start = span
        .marker_ranges
        .first()
        .map_or(span.content_range.start, |m| m.start);
    let end = span
        .marker_ranges
        .last()
        .map_or(span.content_range.end, |m| m.end);
    start..end
}

fn scan_links(text: &str, range: Range<usize>, excluded: &[Range<usize>], spans: &mut Vec<Span>) {
    let slice = &text[range.clone()];
    let mut search_from = 0usize;
    while let Some(bracket_offset) = slice[search_from..].find('[') {
        let bracket = search_from + bracket_offset;
        let absolute_bracket = range.start + bracket;
        if is_excluded(absolute_bracket, excluded) || is_escaped(text, absolute_bracket) {
            search_from = bracket + 1;
            continue;
        }
        let Some(close_bracket_rel) = slice[bracket + 1..].find(']') else {
            break;
        };
        let close_bracket = bracket + 1 + close_bracket_rel;
        if slice.as_bytes()[close_bracket + 1..].first() != Some(&b'(') {
            search_from = bracket + 1;
            continue;
        }
        let paren_start = close_bracket + 1;
        let Some(close_paren_rel) = slice[paren_start + 1..].find(')') else {
            break;
        };
        let close_paren = paren_start + 1 + close_paren_rel;

        let text_content = (range.start + bracket + 1)..(range.start + close_bracket);
        if !text_content.is_empty() {
            spans.push(Span {
                kind: SpanKind::Link,
                marker_ranges: vec![
                    range.start + bracket..range.start + bracket + 1,
                    range.start + close_bracket..range.start + close_paren + 1,
                ],
                content_range: text_content,
            });
        }
        search_from = close_paren + 1;
    }
}

fn is_excluded(offset: usize, excluded: &[Range<usize>]) -> bool {
    excluded.iter().any(|range| range.contains(&offset))
}

fn is_escaped(text: &str, offset: usize) -> bool {
    offset > 0 && text.as_bytes().get(offset - 1) == Some(&b'\\')
}

/// A run of consecutive, unescaped, unexcluded `*` characters.
struct StarRun {
    range: Range<usize>,
}

fn star_runs(text: &str, range: Range<usize>, excluded: &[Range<usize>]) -> Vec<StarRun> {
    let mut runs = Vec::new();
    let bytes = text.as_bytes();
    let mut i = range.start;
    while i < range.end {
        if bytes[i] == b'*' && !is_excluded(i, excluded) && !is_escaped(text, i) {
            let start = i;
            while i < range.end && bytes[i] == b'*' && !is_excluded(i, excluded) {
                i += 1;
            }
            runs.push(StarRun { range: start..i });
        } else {
            i += 1;
        }
    }
    runs
}

/// Finds bold/italic/bold+italic spans in `range`, with exactly one level
/// of nesting in either direction (bold containing italic, or italic
/// containing bold) - enough for the common "toggle bold, then also toggle
/// italic, then untoggle italic" sequence a formatting toolbar produces,
/// without a full recursive delimiter-stack parser. Deeper nesting is left
/// unstyled rather than guessed at.
fn scan_emphasis(
    text: &str,
    range: Range<usize>,
    excluded: &[Range<usize>],
    spans: &mut Vec<Span>,
) {
    let runs = star_runs(text, range.clone(), excluded);
    let mut consumed = vec![false; runs.len()];

    // Pass 1: `***bold+italic***` - a run of exactly 3 stars paired with
    // another run of exactly 3 stars.
    let mut i = 0;
    while i < runs.len() {
        if !consumed[i]
            && run_len(&runs[i]) == 3
            && let Some(j) = (i + 1..runs.len()).find(|&j| !consumed[j] && run_len(&runs[j]) == 3)
        {
            let content = runs[i].range.end..runs[j].range.start;
            if !content.is_empty() {
                spans.push(Span {
                    kind: SpanKind::Bold,
                    marker_ranges: vec![runs[i].range.clone(), runs[j].range.clone()],
                    content_range: content.clone(),
                });
                spans.push(Span {
                    kind: SpanKind::Italic,
                    marker_ranges: vec![],
                    content_range: content,
                });
                consumed[i] = true;
                consumed[j] = true;
            }
        }
        i += 1;
    }

    // Pass 2: `**bold**`, recursing into the content for nested `*italic*`.
    let mut i = 0;
    while i < runs.len() {
        if !consumed[i]
            && run_len(&runs[i]) == 2
            && let Some(j) = (i + 1..runs.len()).find(|&j| !consumed[j] && run_len(&runs[j]) == 2)
        {
            let content = runs[i].range.end..runs[j].range.start;
            if !content.is_empty() {
                spans.push(Span {
                    kind: SpanKind::Bold,
                    marker_ranges: vec![runs[i].range.clone(), runs[j].range.clone()],
                    content_range: content.clone(),
                });
                let mut nested_excluded = excluded.to_vec();
                nested_excluded.push(runs[i].range.clone());
                nested_excluded.push(runs[j].range.clone());
                scan_single_star_italic(text, content.clone(), &nested_excluded, spans);
                consumed[i] = true;
                consumed[j] = true;
                // Runs fully inside the matched bold content are handled
                // by the nested scan above; mark them consumed too so
                // pass 3 does not also try to match them as top-level
                // italic.
                for (k, run) in runs.iter().enumerate() {
                    if content.contains(&run.range.start) {
                        consumed[k] = true;
                    }
                }
            }
        }
        i += 1;
    }

    // Pass 3: whatever single-`*` runs remain are top-level italic,
    // recursing into their content for nested `**bold**`.
    let remaining_excluded: Vec<Range<usize>> = excluded
        .iter()
        .cloned()
        .chain(
            runs.iter()
                .enumerate()
                .filter(|(k, _)| consumed[*k])
                .map(|(_, run)| run.range.clone()),
        )
        .collect();
    let mut i = 0;
    while i < runs.len() {
        if !consumed[i]
            && run_len(&runs[i]) == 1
            && let Some(j) = (i + 1..runs.len()).find(|&j| !consumed[j] && run_len(&runs[j]) == 1)
        {
            let content = runs[i].range.end..runs[j].range.start;
            if !content.is_empty() {
                spans.push(Span {
                    kind: SpanKind::Italic,
                    marker_ranges: vec![runs[i].range.clone(), runs[j].range.clone()],
                    content_range: content.clone(),
                });
                let mut nested_excluded = remaining_excluded.clone();
                nested_excluded.push(runs[i].range.clone());
                nested_excluded.push(runs[j].range.clone());
                scan_double_star_bold(text, content, &nested_excluded, spans);
                consumed[i] = true;
                consumed[j] = true;
            }
        }
        i += 1;
    }

    scan_strike_and_highlight(text, range, excluded, spans);
}

fn run_len(run: &StarRun) -> usize {
    (run.range.end - run.range.start).min(3)
}

fn scan_single_star_italic(
    text: &str,
    range: Range<usize>,
    excluded: &[Range<usize>],
    spans: &mut Vec<Span>,
) {
    let runs = star_runs(text, range, excluded);
    let single_runs: Vec<&StarRun> = runs.iter().filter(|run| run_len(run) == 1).collect();
    let mut i = 0;
    while i + 1 < single_runs.len() {
        let open = single_runs[i];
        let close = single_runs[i + 1];
        let content = open.range.end..close.range.start;
        if !content.is_empty() {
            spans.push(Span {
                kind: SpanKind::Italic,
                marker_ranges: vec![open.range.clone(), close.range.clone()],
                content_range: content,
            });
        }
        i += 2;
    }
}

fn scan_double_star_bold(
    text: &str,
    range: Range<usize>,
    excluded: &[Range<usize>],
    spans: &mut Vec<Span>,
) {
    let runs = star_runs(text, range, excluded);
    let double_runs: Vec<&StarRun> = runs.iter().filter(|run| run_len(run) == 2).collect();
    let mut i = 0;
    while i + 1 < double_runs.len() {
        let open = double_runs[i];
        let close = double_runs[i + 1];
        let content = open.range.end..close.range.start;
        if !content.is_empty() {
            spans.push(Span {
                kind: SpanKind::Bold,
                marker_ranges: vec![open.range.clone(), close.range.clone()],
                content_range: content,
            });
        }
        i += 2;
    }
}

fn scan_strike_and_highlight(
    text: &str,
    range: Range<usize>,
    excluded: &[Range<usize>],
    spans: &mut Vec<Span>,
) {
    for (delimiter, kind) in [("~~", SpanKind::Strikethrough), ("==", SpanKind::Highlight)] {
        for matched in find_delimited(text, range.clone(), delimiter, delimiter, |offset| {
            !is_excluded(offset, excluded)
        }) {
            let content = matched.start + delimiter.len()..matched.end - delimiter.len();
            if !content.is_empty() {
                spans.push(Span {
                    kind,
                    marker_ranges: vec![
                        matched.start..matched.start + delimiter.len(),
                        matched.end - delimiter.len()..matched.end,
                    ],
                    content_range: content,
                });
            }
        }
    }
}

/// Finds non-overlapping `open ... close` delimited spans in `range`,
/// left-to-right, skipping any candidate delimiter for which `allowed`
/// returns `false` or that is escaped with a backslash. Returns the full
/// range (including both delimiters) of each match.
fn find_delimited(
    text: &str,
    range: Range<usize>,
    open: &str,
    close: &str,
    allowed: impl Fn(usize) -> bool,
) -> Vec<Range<usize>> {
    let mut matches = Vec::new();
    let mut cursor = range.start;
    while let Some(open_at) = find_from(text, cursor, range.end, open, &allowed) {
        let search_from = open_at + open.len();
        let Some(close_at) = find_from(text, search_from, range.end, close, &allowed) else {
            break;
        };
        matches.push(open_at..close_at + close.len());
        cursor = close_at + close.len();
    }
    matches
}

fn find_from(
    text: &str,
    from: usize,
    end: usize,
    needle: &str,
    allowed: &impl Fn(usize) -> bool,
) -> Option<usize> {
    let mut search_from = from;
    while search_from < end {
        let relative = text[search_from..end].find(needle)?;
        let at = search_from + relative;
        if allowed(at) && !is_escaped(text, at) {
            return Some(at);
        }
        search_from = at + 1;
    }
    None
}
