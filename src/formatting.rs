#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatAction {
    Normal,
    Heading1,
    Heading2,
    Heading3,
    Bold,
    Italic,
    Strikethrough,
    Highlight,
    InlineCode,
    CodeBlock,
    Quote,
    BulletedList,
    NumberedList,
    Checklist,
    Link,
    HorizontalDivider,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatEdit {
    pub text: String,
    pub selection_start: usize,
    pub selection_end: usize,
}

/// Inline wrap markers. `character` is the single Markdown character the marker
/// is built from and `len` is how many of them a full marker uses. Bold and
/// italic share the `*` character and are told apart by run parity so that
/// toggling one never rewrites the other (`**word**` + italic must not become
/// `*word*`).
#[derive(Clone, Copy)]
struct Marker {
    character: char,
    len: usize,
}

impl Marker {
    const BOLD: Self = Self {
        character: '*',
        len: 2,
    };
    const ITALIC: Self = Self {
        character: '*',
        len: 1,
    };
    const STRIKETHROUGH: Self = Self {
        character: '~',
        len: 2,
    };
    const HIGHLIGHT: Self = Self {
        character: '=',
        len: 2,
    };
    const INLINE_CODE: Self = Self {
        character: '`',
        len: 1,
    };

    fn text(&self) -> String {
        std::iter::repeat_n(self.character, self.len).collect()
    }

    /// Whether a run of `count` adjacent marker characters means *this* marker is
    /// already applied. Italic (`*`) is only present when the run is odd; a plain
    /// `**` run is bold with no italic. Every other marker just needs its length.
    fn present_for_run(&self, count: usize) -> bool {
        if self.character == '*' && self.len == 1 {
            !count.is_multiple_of(2)
        } else {
            count >= self.len
        }
    }
}

pub fn apply_markdown_format(
    source: &str,
    selection_start: usize,
    selection_end: usize,
    action: FormatAction,
) -> FormatEdit {
    let start = floor_char_boundary(source, selection_start.min(source.len()));
    let end = floor_char_boundary(source, selection_end.min(source.len()).max(start));
    match action {
        FormatAction::Normal => heading(source, start, end, ""),
        FormatAction::Heading1 => heading(source, start, end, "# "),
        FormatAction::Heading2 => heading(source, start, end, "## "),
        FormatAction::Heading3 => heading(source, start, end, "### "),
        FormatAction::Bold => toggle_wrap(source, start, end, Marker::BOLD, "bold text"),
        FormatAction::Italic => toggle_wrap(source, start, end, Marker::ITALIC, "italic text"),
        FormatAction::Strikethrough => {
            toggle_wrap(source, start, end, Marker::STRIKETHROUGH, "struck text")
        }
        FormatAction::Highlight => {
            toggle_wrap(source, start, end, Marker::HIGHLIGHT, "highlighted text")
        }
        FormatAction::InlineCode => toggle_wrap(source, start, end, Marker::INLINE_CODE, "code"),
        FormatAction::CodeBlock => wrap(source, start, end, "```\n", "\n```", "code"),
        FormatAction::Link => wrap(source, start, end, "[", "](https://)", "link text"),
        FormatAction::Quote => prefix_lines(source, start, end, "> "),
        FormatAction::BulletedList => prefix_lines(source, start, end, "- "),
        FormatAction::NumberedList => prefix_lines(source, start, end, "1. "),
        FormatAction::Checklist => prefix_lines(source, start, end, "- [ ] "),
        FormatAction::HorizontalDivider => insert(source, start, "\n---\n"),
    }
}

/// Semantic toggle for a symmetric inline marker.
///
/// The same button applies the marker when it is absent and removes it when the
/// current selection (or the span the cursor sits in) is already wrapped, so
/// repeated presses never stack `**`/`*`/`~~`/`==` pairs.
fn toggle_wrap(
    source: &str,
    start: usize,
    end: usize,
    marker: Marker,
    placeholder: &str,
) -> FormatEdit {
    let text = marker.text();

    // 1. The selection itself spans the markers: `**word**` selected whole.
    if end - start >= 2 * marker.len {
        let selected = &source[start..end];
        let lead = leading_run(selected, marker.character);
        let trail = trailing_run(selected, marker.character);
        let inner = &selected[marker.len..selected.len() - marker.len];
        if marker.present_for_run(lead.min(trail))
            && !inner.is_empty()
            && !edge_conflicts(marker, inner)
        {
            let mut result = String::with_capacity(source.len() - 2 * marker.len);
            result.push_str(&source[..start]);
            result.push_str(inner);
            result.push_str(&source[end..]);
            let selection_start = start;
            let selection_end = start + inner.len();
            return FormatEdit {
                text: result,
                selection_start,
                selection_end,
            };
        }
    }

    // 2. The markers sit immediately outside the selection: `**` `word` `**`.
    let before = trailing_run(&source[..start], marker.character);
    let after = leading_run(&source[end..], marker.character);
    if start != end
        && marker.present_for_run(before.min(after))
        && !lone_star_conflict(marker, source, start, end)
    {
        let mut result = String::with_capacity(source.len() - 2 * marker.len);
        result.push_str(&source[..start - marker.len]);
        result.push_str(&source[start..end]);
        result.push_str(&source[end + marker.len..]);
        let selection_start = start - marker.len;
        let selection_end = end - marker.len;
        return FormatEdit {
            text: result,
            selection_start,
            selection_end,
        };
    }

    // 3. No selection but the cursor is inside a marked span on this line.
    if start == end
        && let Some(edit) = toggle_at_cursor(source, start, marker)
    {
        return edit;
    }

    // 4. Nothing to remove: wrap the selection (or a placeholder).
    wrap(source, start, end, &text, &text, placeholder)
}

/// For italic specifically, refuse to treat a `**` boundary as an italic marker
/// so that toggling italic on bold text nests rather than rewriting the bold.
fn lone_star_conflict(marker: Marker, source: &str, start: usize, end: usize) -> bool {
    if !(marker.character == '*' && marker.len == 1) {
        return false;
    }
    let before = trailing_run(&source[..start], '*');
    let after = leading_run(&source[end..], '*');
    // An even run is pure bold: there is no italic marker to remove.
    before.is_multiple_of(2) || after.is_multiple_of(2)
}

fn edge_conflicts(marker: Marker, inner: &str) -> bool {
    marker.character == '*' && marker.len == 1 && (inner.starts_with('*') || inner.ends_with('*'))
}

fn toggle_at_cursor(source: &str, cursor: usize, marker: Marker) -> Option<FormatEdit> {
    let line_start = source[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let line_end = source[cursor..]
        .find('\n')
        .map_or(source.len(), |index| cursor + index);
    let text = marker.text();
    let left_relative = source[line_start..cursor].rfind(&text)?;
    let right_relative = source[cursor..line_end].find(&text)?;
    let left = line_start + left_relative;
    let right = cursor + right_relative;
    let inner = &source[left + marker.len..right];
    if inner.is_empty() || inner.contains('\n') || inner.contains(marker.character) {
        return None;
    }
    // Reject a `**` boundary that only looks like an italic marker.
    if marker.character == '*' && marker.len == 1 {
        let left_char = source[..left].chars().next_back();
        let right_char = source[right + marker.len..].chars().next();
        if left_char == Some('*') || right_char == Some('*') {
            return None;
        }
    }
    let mut result = String::with_capacity(source.len() - 2 * marker.len);
    result.push_str(&source[..left]);
    result.push_str(inner);
    result.push_str(&source[right + marker.len..]);
    let selection_start = left;
    let selection_end = left + inner.len();
    Some(FormatEdit {
        text: result,
        selection_start,
        selection_end,
    })
}

fn leading_run(value: &str, character: char) -> usize {
    value
        .chars()
        .take_while(|candidate| *candidate == character)
        .count()
}

fn trailing_run(value: &str, character: char) -> usize {
    value
        .chars()
        .rev()
        .take_while(|candidate| *candidate == character)
        .count()
}

fn wrap(
    source: &str,
    start: usize,
    end: usize,
    before: &str,
    after: &str,
    placeholder: &str,
) -> FormatEdit {
    let selected = if start == end {
        placeholder
    } else {
        &source[start..end]
    };
    let mut text =
        String::with_capacity(source.len() + before.len() + after.len() + selected.len());
    text.push_str(&source[..start]);
    text.push_str(before);
    let selection_start = text.len();
    text.push_str(selected);
    let selection_end = text.len();
    text.push_str(after);
    text.push_str(&source[end..]);
    FormatEdit {
        text,
        selection_start,
        selection_end,
    }
}

/// Line-level heading toggle.
///
/// Any existing heading prefix on the affected lines is stripped before the new
/// one is applied, so pressing Heading 1 on `## Title` yields `# Title` rather
/// than `# ## Title`. Pressing the same level again (or Normal text) removes the
/// prefix and returns the block to a paragraph.
fn heading(source: &str, start: usize, end: usize, prefix: &str) -> FormatEdit {
    let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
    let line_end = source[end..]
        .find('\n')
        .map_or(source.len(), |index| end + index);
    let block = &source[line_start..line_end];

    let already_at_level = !prefix.is_empty()
        && block
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .all(|line| line_heading_prefix(line) == prefix);
    let effective_prefix = if already_at_level { "" } else { prefix };

    let mut replaced = String::new();
    for (index, line) in block.split('\n').enumerate() {
        if index > 0 {
            replaced.push('\n');
        }
        replaced.push_str(effective_prefix);
        replaced.push_str(line.trim_start_matches('#').trim_start());
    }
    replace_block(source, line_start, line_end, replaced)
}

/// Returns the `# `/`## `/`### ` prefix a line already carries, or `""`.
fn line_heading_prefix(line: &str) -> &'static str {
    let hashes = line
        .chars()
        .take_while(|character| *character == '#')
        .count();
    let rest = &line[hashes..];
    match (hashes, rest.starts_with(' ')) {
        (1, true) => "# ",
        (2, true) => "## ",
        (3, true) => "### ",
        _ => "",
    }
}

fn prefix_lines(source: &str, start: usize, end: usize, prefix: &str) -> FormatEdit {
    let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
    let line_end = source[end..]
        .find('\n')
        .map_or(source.len(), |index| end + index);
    let replaced = source[line_start..line_end]
        .split('\n')
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    replace_block(source, line_start, line_end, replaced)
}

fn replace_block(source: &str, start: usize, end: usize, replacement: String) -> FormatEdit {
    let mut text = String::with_capacity(source.len() + replacement.len());
    text.push_str(&source[..start]);
    text.push_str(&replacement);
    text.push_str(&source[end..]);
    FormatEdit {
        text,
        selection_start: start,
        selection_end: start + replacement.len(),
    }
}

fn insert(source: &str, at: usize, inserted: &str) -> FormatEdit {
    let mut text = String::with_capacity(source.len() + inserted.len());
    text.push_str(&source[..at]);
    text.push_str(inserted);
    text.push_str(&source[at..]);
    FormatEdit {
        text,
        selection_start: at + inserted.len(),
        selection_end: at + inserted.len(),
    }
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}
