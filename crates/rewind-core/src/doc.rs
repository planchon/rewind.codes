//! A script's title and description, from the comment at its top (`archi/04-tools.md`,
//! "Script titles"): a block comment (`/** … */` or `/* … */`) or a run of `//` lines, before any
//! code. Its first line that is not blank is the title; the lines after it, the description.

/// The longest title and description kept, in characters.
pub const MAX_TITLE_CHARS: usize = 120;
pub const MAX_DESCRIPTION_CHARS: usize = 2_000;

/// What the comment at the top of a script says of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptDoc {
    pub title: String,
    pub description: Option<String>,
}

/// The title and description of the comment at the top of `source`, if it has one with text.
pub fn script_doc(source: &str) -> Option<ScriptDoc> {
    let source = source.trim_start_matches('\u{feff}');
    let mut rest = source.trim_start();
    // A shebang is not the comment.
    if rest.starts_with("#!") {
        rest = rest
            .split_once('\n')
            .map_or("", |(_, after)| after)
            .trim_start();
    }
    let lines: Vec<String> = if let Some(body) = rest.strip_prefix("/*") {
        let (body, _) = body.split_once("*/")?;
        body.trim_start_matches('*')
            .lines()
            .map(|line| {
                let line = line.trim();
                line.strip_prefix('*').unwrap_or(line).trim().to_owned()
            })
            .collect()
    } else if rest.starts_with("//") {
        rest.lines()
            .map(str::trim)
            .take_while(|line| line.starts_with("//"))
            .map(|line| line.trim_start_matches('/').trim().to_owned())
            .collect()
    } else {
        return None;
    };
    // A tag (`@ts-check`, `@param`) is not prose.
    let lines: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|line| !line.starts_with('@'))
        .collect();
    let first = lines.iter().position(|line| !line.is_empty())?;
    let title = cut(lines[first], MAX_TITLE_CHARS);
    let description = lines[first + 1..]
        .iter()
        .skip_while(|line| line.is_empty())
        .fold(Vec::<String>::new(), |mut paragraphs, line| {
            match (line.is_empty(), paragraphs.last_mut()) {
                (true, Some(last)) if !last.is_empty() => paragraphs.push(String::new()),
                (true, _) => {}
                (false, Some(last)) if !last.is_empty() => {
                    last.push(' ');
                    last.push_str(line);
                }
                (false, Some(last)) => last.push_str(line),
                (false, None) => paragraphs.push((*line).to_owned()),
            }
            paragraphs
        })
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(ScriptDoc {
        title,
        description: (!description.is_empty()).then(|| cut(&description, MAX_DESCRIPTION_CHARS)),
    })
}

fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", text[..at].trim_end()),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_comment_gives_the_title_then_the_description() {
        let doc = script_doc(
            "/**\n * Triage new GitHub issues\n *\n * Labels each new issue, and asks for an\n * approval.\n *\n * Then closes duplicates.\n */\nimport { github } from \"rewind:tools\";\n",
        )
        .expect("a doc");
        assert_eq!(doc.title, "Triage new GitHub issues");
        assert_eq!(
            doc.description.as_deref(),
            Some("Labels each new issue, and asks for an approval.\n\nThen closes duplicates.")
        );
    }

    #[test]
    fn line_comments_count_until_the_code() {
        let doc = script_doc(
            "// Daily digest\n// Posts the day's issues.\nimport x from \"y\";\n// later",
        )
        .expect("a doc");
        assert_eq!(doc.title, "Daily digest");
        assert_eq!(doc.description.as_deref(), Some("Posts the day's issues."));
        assert_eq!(
            script_doc("/* One line */ export default 1")
                .map(|d| d.title)
                .as_deref(),
            Some("One line")
        );
    }

    #[test]
    fn code_first_or_an_empty_comment_has_no_title() {
        assert_eq!(
            script_doc("import { github } from \"rewind:tools\";\n/** late */"),
            None
        );
        assert_eq!(script_doc("/** */\nexport default 1"), None);
        assert_eq!(script_doc("// @ts-check\nexport default 1"), None);
        assert_eq!(script_doc(""), None);
    }

    #[test]
    fn a_long_title_is_cut() {
        let doc = script_doc(&format!("// {}", "a".repeat(300))).expect("a doc");
        assert_eq!(doc.title.chars().count(), MAX_TITLE_CHARS + 1);
        assert!(doc.title.ends_with('…'));
    }
}
