//! Positions in a source text, code frames, and the triple-slash directives of a file.
//!
//! Lines end as TypeScript ends them: `\n`, `\r\n`, `\r`, U+2028 and U+2029. Columns count
//! Unicode characters, 1-based; tsgo counts UTF-16 code units, and its columns are converted.

use std::borrow::Cow;

/// Characters of a line shown in a frame before it is cut around the position.
const MAX_LINE: usize = 160;
/// Characters kept before the position in a line that is cut.
const BEFORE_POSITION: usize = 60;

/// A text and the byte offsets where its lines start.
#[derive(Debug)]
pub(crate) struct SourceText<'a> {
    text: &'a str,
    /// Byte offset of each line's first character.
    starts: Vec<usize>,
}

impl<'a> SourceText<'a> {
    pub(crate) fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        let mut chars = text.char_indices().peekable();
        while let Some((offset, c)) = chars.next() {
            match c {
                '\r' => {
                    if chars.peek().is_some_and(|&(_, next)| next == '\n') {
                        chars.next();
                        starts.push(offset + 2);
                    } else {
                        starts.push(offset + 1);
                    }
                }
                '\n' | '\u{2028}' | '\u{2029}' => starts.push(offset + c.len_utf8()),
                _ => {}
            }
        }
        Self { text, starts }
    }

    /// The bytes of a line without its terminator; `index` is 0-based.
    fn line_range(&self, index: usize) -> (usize, usize) {
        let start = self.starts[index];
        let Some(&next) = self.starts.get(index + 1) else {
            return (start, self.text.len());
        };
        let line = &self.text[start..next];
        let terminator = if line.ends_with("\r\n") {
            2
        } else {
            line.chars().next_back().map_or(0, char::len_utf8)
        };
        (start, next - terminator)
    }

    fn line(&self, index: usize) -> &'a str {
        let (start, end) = self.line_range(index);
        &self.text[start..end]
    }

    /// The 0-based line holding a byte offset.
    fn line_index(&self, offset: usize) -> usize {
        self.starts.partition_point(|&start| start <= offset) - 1
    }

    /// Line and column of a byte offset, both 1-based; the column counts characters.
    pub(crate) fn position(&self, offset: usize) -> (u32, u32) {
        let offset = self.floor(offset);
        let index = self.line_index(offset);
        let (start, end) = self.line_range(index);
        let column = self.text[start..offset.min(end)].chars().count();
        (to_u32(index + 1), to_u32(column + 1))
    }

    /// The byte offset of a position as tsgo reports it: a 1-based line and a 1-based column in
    /// UTF-16 code units. A position past the end of its line is the end of the line.
    pub(crate) fn offset_of_utf16(&self, line: u32, column: u32) -> usize {
        let index = (line.max(1) as usize - 1).min(self.starts.len() - 1);
        let (start, end) = self.line_range(index);
        let mut units = column.max(1) as usize - 1;
        for (offset, c) in self.text[start..end].char_indices() {
            if units == 0 {
                return start + offset;
            }
            units = units.saturating_sub(c.len_utf16());
        }
        end
    }

    /// The end of the token that starts at `offset`, on its line: an identifier, a number, a
    /// string, or one character. tsgo gives where a problem starts, not where it ends.
    pub(crate) fn token_end(&self, offset: usize) -> usize {
        let offset = self.floor(offset);
        let (_, line_end) = self.line_range(self.line_index(offset));
        if offset >= line_end {
            return offset;
        }
        let rest = &self.text[offset..line_end];
        let mut chars = rest.char_indices();
        let Some((_, first)) = chars.next() else {
            return offset;
        };
        let length = if is_identifier_start(first) || first == '#' {
            rest.char_indices()
                .skip(1)
                .find(|&(_, c)| !is_identifier_part(c))
                .map_or(rest.len(), |(at, _)| at)
        } else if first.is_ascii_digit() {
            rest.char_indices()
                .find(|&(_, c)| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
                .map_or(rest.len(), |(at, _)| at)
        } else if matches!(first, '"' | '\'' | '`') {
            let mut escaped = false;
            chars
                .find(|&(_, c)| {
                    let closes = !escaped && c == first;
                    escaped = !escaped && c == '\\';
                    closes
                })
                .map_or(rest.len(), |(at, c)| at + c.len_utf8())
        } else {
            first.len_utf8()
        };
        offset + length
    }

    /// A few lines around `start`: the line before and the line after when they are not
    /// blank, and the span from `start` to `end` underlined on its first line.
    ///
    /// ```text
    /// 2 |   const repo = input.repo;
    /// 3 |   await github.list_issues({ repo: 42 });
    ///   |                                    ^^
    /// 4 | }
    /// ```
    pub(crate) fn frame(&self, start: usize, end: usize) -> String {
        let start = self.floor(start);
        let index = self.line_index(start);
        let (line_start, line_end) = self.line_range(index);
        let before = index
            .checked_sub(1)
            .filter(|&previous| !self.line(previous).trim().is_empty());
        let after = Some(index + 1)
            .filter(|&next| next < self.starts.len() && !self.line(next).trim().is_empty());
        let last = after.unwrap_or(index) + 1;
        let width = last.to_string().len();

        let line = self.line(index);
        let column = self.text[line_start..start.min(line_end)].chars().count();
        let span = self.text[start.min(line_end)..self.floor(end).clamp(start, line_end)]
            .chars()
            .count()
            .max(1);
        let (shown, column) = cut(line, column);

        let mut frame = Vec::with_capacity(4);
        if let Some(previous) = before {
            frame.push(numbered(
                previous + 1,
                width,
                &cut(self.line(previous), 0).0,
            ));
        }
        frame.push(numbered(index + 1, width, &shown));
        // Tabs stay tabs, so the marks line up with the line above.
        let padding: String = shown
            .chars()
            .take(column)
            .map(|c| if c == '\t' { '\t' } else { ' ' })
            .collect();
        let marks = "^".repeat(span.min(shown.chars().count().saturating_sub(column).max(1)));
        frame.push(format!("{:width$} | {padding}{marks}", ""));
        if let Some(next) = after {
            frame.push(numbered(next + 1, width, &cut(self.line(next), 0).0));
        }
        frame.join("\n")
    }

    /// The largest char boundary at or before `offset`, within the text.
    fn floor(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }
}

fn numbered(number: usize, width: usize, line: &str) -> String {
    format!("{number:>width$} | {line}").trim_end().to_owned()
}

/// A line short enough for a frame: a long one is cut around `column` (0-based, in
/// characters), with `…` where text was left out. Returns the line and the column in it.
fn cut(line: &str, column: usize) -> (Cow<'_, str>, usize) {
    let length = line.chars().count();
    if length <= MAX_LINE {
        return (Cow::Borrowed(line), column);
    }
    let first = column
        .saturating_sub(BEFORE_POSITION)
        .min(length - MAX_LINE);
    let mut shown = String::new();
    let mut column = column - first;
    if first > 0 {
        shown.push('…');
        column += 1;
    }
    shown.extend(line.chars().skip(first).take(MAX_LINE));
    if first + MAX_LINE < length {
        shown.push('…');
    }
    (Cow::Owned(shown), column)
}

fn is_identifier_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

fn is_identifier_part(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || c == '\u{200c}' || c == '\u{200d}'
}

fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// A triple-slash reference directive: `/// <reference path="fetch.d.ts" />`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Directive {
    /// `path`, `lib`, `types` or `no-default-lib`.
    pub(crate) kind: String,
    pub(crate) value: String,
}

/// Blanks the triple-slash reference directives of a file, keeping every other character, so
/// positions do not move. TypeScript reads them only in the file's prologue, the comments
/// before its first token, and only there are they looked for. Returns the text and the
/// directives it held.
pub(crate) fn blank_directives(text: &str) -> (Cow<'_, str>, Vec<Directive>) {
    let mut directives = Vec::new();
    let mut blanked: Vec<(usize, usize)> = Vec::new();
    let mut at = 0;
    if text.starts_with('\u{feff}') {
        at = '\u{feff}'.len_utf8();
    }
    if text[at..].starts_with("#!") {
        at = line_end(text, at);
    }
    loop {
        let rest = &text[at..];
        let trimmed = rest.trim_start();
        at += rest.len() - trimmed.len();
        if trimmed.starts_with("//") {
            let end = line_end(text, at);
            let comment = &text[at..end];
            if let Some(directive) = comment.strip_prefix("///").and_then(reference) {
                directives.push(directive);
                blanked.push((at, end));
            }
            at = end;
        } else if let Some(comment) = trimmed.strip_prefix("/*") {
            match comment.find("*/") {
                Some(close) => at += 2 + close + 2,
                None => break,
            }
        } else {
            break;
        }
    }
    if blanked.is_empty() {
        return (Cow::Borrowed(text), directives);
    }
    let mut result = String::with_capacity(text.len());
    let mut copied = 0;
    for (start, end) in blanked {
        result.push_str(&text[copied..start]);
        result.extend(std::iter::repeat_n(' ', text[start..end].chars().count()));
        copied = end;
    }
    result.push_str(&text[copied..]);
    (Cow::Owned(result), directives)
}

/// The offset of the end of the line holding `at`, before its terminator.
fn line_end(text: &str, at: usize) -> usize {
    text[at..]
        .find(['\n', '\r', '\u{2028}', '\u{2029}'])
        .map_or(text.len(), |end| at + end)
}

/// Reads `<reference kind="value" />`, the text of a comment after `///`.
fn reference(comment: &str) -> Option<Directive> {
    let rest = comment.trim_start().strip_prefix("<reference")?;
    if !rest.starts_with(|c: char| c.is_whitespace() || c == '/' || c == '>') {
        return None;
    }
    let rest = rest.trim_start();
    let (kind, rest) = rest.split_once('=')?;
    let rest = rest.trim_start();
    let quote = rest.chars().next().filter(|&c| c == '"' || c == '\'')?;
    let (value, _) = rest[1..].split_once(quote)?;
    Some(Directive {
        kind: kind.trim().to_owned(),
        value: value.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_end_as_in_typescript() {
        let source = "a\r\nb\rc\u{2028}d\ne";
        let text = SourceText::new(source);
        let positions: Vec<_> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|letter| text.position(source.find(letter).unwrap()))
            .collect();
        assert_eq!(positions, [(1, 1), (2, 1), (3, 1), (4, 1), (5, 1)]);
        assert_eq!(text.line(0), "a");
        assert_eq!(text.line(1), "b");
        assert_eq!(text.line(3), "d");
    }

    #[test]
    fn columns_count_characters_and_tsgo_columns_count_utf16_units() {
        let source = "x\n  const s = \"😀é\"; const n: string = 42;";
        let text = SourceText::new(source);
        let offset = source.find("42").unwrap();
        // UTF-16: the emoji is two units, `é` one.
        let utf16 = "  const s = \"😀é\"; const n: string = "
            .encode_utf16()
            .count() as u32
            + 1;
        assert_eq!(text.offset_of_utf16(2, utf16), offset);
        assert_eq!(text.position(offset), (2, 37));
        // Past the end of a line: the end of the line.
        assert_eq!(text.offset_of_utf16(1, 99), 1);
        assert_eq!(text.offset_of_utf16(99, 1), source.find("  const").unwrap());
    }

    #[test]
    fn tokens_are_identifiers_numbers_strings_or_one_character() {
        let source = "foo.bar_1(42.5, \"a\\\"b\", 'c', { x }); $é";
        let text = SourceText::new(source);
        let token = |needle: &str| {
            let start = source.find(needle).unwrap();
            &source[start..text.token_end(start)]
        };
        assert_eq!(token("foo"), "foo");
        assert_eq!(token("bar_1"), "bar_1");
        assert_eq!(token("42.5"), "42.5");
        assert_eq!(token("\"a"), "\"a\\\"b\"");
        assert_eq!(token("'c'"), "'c'");
        assert_eq!(token("{ x"), "{");
        assert_eq!(token("$é"), "$é");
        assert_eq!(text.token_end(source.len()), source.len());
    }

    #[test]
    fn frames_show_the_line_with_its_neighbours_and_mark_the_span() {
        let source = "import { github } from \"ptc:tools\";\n\nexport default async function main() {\n  await github.list_issues({ repo: 42 });\n}\n";
        let text = SourceText::new(source);
        let start = source.find("42").unwrap();
        assert_eq!(
            text.frame(start, start + 2),
            "3 | export default async function main() {\n4 |   await github.list_issues({ repo: 42 });\n  |                                    ^^\n5 | }"
        );
        // The first line has no line before; a blank line is not shown.
        assert_eq!(
            text.frame(0, 6),
            "1 | import { github } from \"ptc:tools\";\n  | ^^^^^^"
        );
        // An empty span is marked by one character.
        assert_eq!(text.frame(source.len(), source.len()), "5 | }\n6 |\n  | ^");
    }

    #[test]
    fn frames_keep_tabs_and_widen_line_numbers() {
        let mut source = "\n".repeat(9);
        source.push_str("\tconst x: string = 1;\n\tx;");
        let text = SourceText::new(&source);
        let start = source.find("x:").unwrap();
        assert_eq!(
            text.frame(start, start + 1),
            "10 | \tconst x: string = 1;\n   | \t      ^\n11 | \tx;"
        );
    }

    #[test]
    fn frames_cut_long_lines_around_the_position() {
        let source = format!("const a = [{}];", "1, ".repeat(100));
        let text = SourceText::new(&source);
        let start = source.rfind('1').unwrap();
        let frame = text.frame(start, start + 1);
        let lines: Vec<&str> = frame.lines().collect();
        assert!(lines[0].starts_with("1 | …"), "{frame}");
        assert!(lines[0].ends_with("1, 1, ];"), "{frame}");
        let marker = lines[1].find('^').unwrap();
        assert_eq!(lines[0].chars().nth(marker), Some('1'), "{frame}");
    }

    #[test]
    fn reference_directives_of_the_prologue_are_blanked() {
        let source = "#!/usr/bin/env node\n/// <reference lib=\"dom\" />\n/* a comment */\n///<reference path='fetch.d.ts'/>\n// a comment\nimport { sleep } from \"ptc\";\n/// <reference types=\"node\" />\n";
        let (blanked, directives) = blank_directives(source);
        assert_eq!(
            directives,
            [
                Directive {
                    kind: "lib".into(),
                    value: "dom".into()
                },
                Directive {
                    kind: "path".into(),
                    value: "fetch.d.ts".into()
                }
            ]
        );
        assert_eq!(blanked.len(), source.len());
        assert!(!blanked.contains("lib=\"dom\""));
        assert!(!blanked.contains("fetch.d.ts"));
        // After the first token, a directive is a comment: TypeScript ignores it.
        assert!(blanked.contains("/// <reference types=\"node\" />"));
        assert_eq!(blanked.lines().count(), source.lines().count());
        // Nothing to blank: the text is borrowed.
        assert!(matches!(
            blank_directives("// <reference lib=\"dom\" />\nlet x;"),
            (Cow::Borrowed(_), ref found) if found.is_empty()
        ));
    }
}
