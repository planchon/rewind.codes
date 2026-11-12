//! Failure groups (`archi/05-testing-and-observability.md`, "Read model"; `archi/08-decisions.md`,
//! "The fingerprint is…").
//!
//! A run that failed, was cancelled or was aborted gets a **signature**: its error's name (an
//! abort's kind) and its message with the parts that vary replaced by placeholders. Its
//! **fingerprint**, the first 16 hexadecimal digits of the SHA-256 of the signature, names its
//! failure group: models write a new script for most conversations, so runs are grouped by
//! cause, across scripts. The projector computes both, so every adapter of the read model stores
//! the same values.
//!
//! The message is rewritten by these rules, in this order, each on what the previous ones left:
//!
//! | Rule | Placeholder | Matches |
//! | --- | --- | --- |
//! | Double-quoted strings | `<str>` | `"…"`, with backslash escapes |
//! | UUIDs | `<uuid>` | 8-4-4-4-12 hexadecimal digits |
//! | ISO dates and times | `<time>` | `2026-11-12`, `2026-11-12T10:00:00.123Z`, `10:00:00+01:00` |
//! | Hexadecimal ids | `<id>` | a run of ASCII letters and digits, 8 long or more, all hexadecimal, with a digit and a letter |
//! | Numbers | `<n>` | `42`, `-3.5`, `1.2.3`, `0x1f`, not preceded by a letter, a digit, `_` or `.` |
//!
//! Single-quoted words are kept: V8 quotes property names (`reading 'id'`), which tell failures
//! apart. The other rules still apply inside single quotes (`'42'` becomes `'<n>'`). A run of
//! decimal digits only is a number, not an id. A number glued after a letter (`v8`, `utf8`,
//! `sha256`) is part of a word and kept; one followed by letters (`5000ms`) is replaced.
//! Whitespace is then collapsed, and the signature, `{name}: {message}`, keeps at most
//! [`SIGNATURE_CHARS`] characters.

use std::sync::LazyLock;

use ptc_core::{Digest, RunOutcome};
use regex::{Captures, Regex};

use crate::ports::Ending;

/// Characters a signature keeps at most.
pub const SIGNATURE_CHARS: usize = 200;

/// Hexadecimal digits of a fingerprint.
pub const FINGERPRINT_DIGITS: usize = 16;

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("the patterns of the failure signatures are valid")
}

static QUOTED: LazyLock<Regex> = LazyLock::new(|| pattern(r#"(?s)"(?:[^"\\]|\\.)*""#));

static UUID: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r"[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}")
});

static TIME: LazyLock<Regex> = LazyLock::new(|| {
    let zone = r"(?:[Zz]|[+-][0-9]{2}:?[0-9]{2})";
    let fraction = r"(?:[.,][0-9]+)";
    pattern(&format!(
        "[0-9]{{4}}-[0-9]{{2}}-[0-9]{{2}}(?:[Tt ][0-9]{{2}}:[0-9]{{2}}(?::[0-9]{{2}}{fraction}?)?{zone}?)?\
         |[0-9]{{2}}:[0-9]{{2}}:[0-9]{{2}}{fraction}?{zone}?"
    ))
});

static WORD: LazyLock<Regex> = LazyLock::new(|| pattern("[0-9A-Za-z]+"));

/// A number, after the character that precedes it (captured, and kept): none at the start of
/// the text.
static NUMBER: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(^|[^\w.])[+-]?(?:0[xX][0-9A-Fa-f]+|[0-9]+(?:\.[0-9]+)*)"));

/// Whether a word is a hexadecimal id: 8 characters or more, all hexadecimal, with a digit
/// (so that words such as `deadbeef` stay) and a letter (so that a long number is a number).
fn is_hex_id(word: &str) -> bool {
    word.len() >= 8
        && word.bytes().all(|byte| byte.is_ascii_hexdigit())
        && word.bytes().any(|byte| byte.is_ascii_digit())
        && word.bytes().any(|byte| byte.is_ascii_alphabetic())
}

/// The signature of an error: `{name}: {message}`, the message's varying parts replaced by
/// placeholders, whitespace collapsed, at most [`SIGNATURE_CHARS`] characters.
pub fn signature(name: &str, message: &str) -> String {
    let message = QUOTED.replace_all(message, "<str>");
    let message = UUID.replace_all(&message, "<uuid>");
    let message = TIME.replace_all(&message, "<time>");
    let message = WORD.replace_all(&message, |word: &Captures<'_>| {
        if is_hex_id(&word[0]) {
            "<id>".to_owned()
        } else {
            word[0].to_owned()
        }
    });
    let message = NUMBER.replace_all(&message, "${1}<n>");
    let signature = format!("{name}: {message}");
    let collapsed = signature.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = collapsed.chars().take(SIGNATURE_CHARS).collect();
    truncated.trim_end().to_owned()
}

/// The fingerprint of a signature: the first 16 lowercase hexadecimal digits of its SHA-256.
pub fn fingerprint(signature: &str) -> String {
    let mut hex = Digest::of(signature.as_bytes()).to_hex();
    hex.truncate(FINGERPRINT_DIGITS);
    hex
}

/// The innermost `script.ts:<line>:<column>` of a message or a stack: the first one, since a V8
/// stack lists the innermost frame first.
pub fn script_location(text: &str) -> Option<(u32, u32)> {
    let mut rest = text;
    while let Some(at) = rest.find("script.ts:") {
        rest = &rest[at + "script.ts:".len()..];
        let mut parts = rest.splitn(3, ':');
        let line = parts.next().and_then(|line| line.parse().ok());
        let column = parts.next().and_then(|column| {
            let digits: String = column.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        });
        if let (Some(line), Some(column)) = (line, column) {
            return Some((line, column));
        }
    }
    None
}

/// How a run that failed, was cancelled or was aborted ended; `None` for a completed run. The
/// line is read from the error's stack, else from its message.
pub fn ending(outcome: &RunOutcome) -> Option<Ending> {
    let (error, message, stack) = match outcome {
        RunOutcome::Completed { .. } => return None,
        RunOutcome::Failed { error } => (
            error.name.as_str(),
            error.message.as_str(),
            error.stack.as_deref(),
        ),
        RunOutcome::Aborted { reason } => (reason.kind.as_str(), reason.message.as_str(), None),
    };
    let line = stack
        .and_then(script_location)
        .or_else(|| script_location(message))
        .map(|(line, _)| line);
    let signature = signature(error, message);
    Some(Ending {
        error: error.to_owned(),
        message: message.to_owned(),
        line,
        fingerprint: fingerprint(&signature),
        signature,
    })
}

#[cfg(test)]
mod tests {
    use ptc_core::{AbortKind, AbortReason, ScriptError};
    use serde_json::json;

    use super::*;

    fn sig(message: &str) -> String {
        signature("Error", message)
    }

    #[test]
    fn property_names_in_single_quotes_tell_failures_apart() {
        let message = "Cannot read properties of undefined (reading 'id')";
        assert_eq!(
            signature("TypeError", message),
            "TypeError: Cannot read properties of undefined (reading 'id')"
        );
        assert_ne!(
            fingerprint(&signature("TypeError", message)),
            fingerprint(&signature(
                "TypeError",
                "Cannot read properties of undefined (reading 'name')"
            ))
        );
        assert_eq!(
            sig("Cannot find module './util'"),
            "Error: Cannot find module './util'"
        );
        assert_eq!(
            sig("no item at index '42'"),
            "Error: no item at index '<n>'"
        );
        assert_eq!(sig("don't retry 3 times"), "Error: don't retry <n> times");
    }

    #[test]
    fn numbers_are_replaced() {
        let at = |n: u32| {
            signature(
                "ToolError",
                &format!("github.close_issue failed: rate limit exceeded on {n} requests"),
            )
        };
        assert_eq!(
            at(42),
            "ToolError: github.close_issue failed: rate limit exceeded on <n> requests"
        );
        assert_eq!(fingerprint(&at(42)), fingerprint(&at(7)));
        assert_eq!(
            sig("expected -3.5 or +2, got 1.2.3 (0x1f)"),
            "Error: expected <n> or <n>, got <n> (<n>)"
        );
        assert_eq!(
            sig("timed out after 5000ms, 12 retries"),
            "Error: timed out after <n>ms, <n> retries"
        );
        assert_eq!(sig("range 1-2, item #7"), "Error: range <n>-<n>, item #<n>");
        assert_eq!(
            sig("s3.put_object needs utf8, v8 and sha256 on x86_64"),
            "Error: s3.put_object needs utf8, v8 and sha256 on x86_64"
        );
        assert_eq!(sig("42 is too large"), "Error: <n> is too large");
        assert_eq!(
            sig("issue 1234567890 not found"),
            "Error: issue <n> not found"
        );
    }

    #[test]
    fn ids_uuids_times_and_strings_are_replaced() {
        assert_eq!(
            sig("run 0193A5C4-e5f6-7a8b-9c0d-1e2f3a4b5c6d not found"),
            "Error: run <uuid> not found"
        );
        assert_eq!(
            sig("deadline 2026-11-12T10:00:00.123Z passed at 10:05:00+01:00 on 2026-11-12"),
            "Error: deadline <time> passed at <time> on <time>"
        );
        assert_eq!(
            sig("at 2026-11-12 10:00:00, 3 left"),
            "Error: at <time>, <n> left"
        );
        assert_eq!(
            sig("request a1b2c3d4e5f6 (req_0193a5c4e5, #ff00aa11) failed"),
            "Error: request <id> (req_<id>, #<id>) failed"
        );
        assert_eq!(
            sig("deadbeef and facade stay, 7f3a is short"),
            "Error: deadbeef and facade stay, <n>f3a is short"
        );
        assert_eq!(
            sig(r#"unknown repo "acme/api 2", expected "a \"b\" c" or "x""#),
            "Error: unknown repo <str>, expected <str> or <str>"
        );
        assert_eq!(
            sig(r#"a "quoted 0193a5c4-e5f6-7a8b-9c0d-1e2f3a4b5c6d" string"#),
            "Error: a <str> string"
        );
        assert_eq!(
            sig(r#"an "unterminated quote"#),
            r#"Error: an "unterminated quote"#
        );
    }

    #[test]
    fn whitespace_is_collapsed_and_long_signatures_are_cut() {
        assert_eq!(sig("  a\n\n  b\tc  "), "Error: a b c");
        assert_eq!(sig(""), "Error:");
        // "Err: word word …": the 200th character is a space.
        let long = signature("Err", &"word ".repeat(100));
        assert_eq!(
            long.chars().count(),
            SIGNATURE_CHARS - 1,
            "cut, then trimmed"
        );
        assert!(long.starts_with("Err: word word") && long.ends_with("word"));
        let accented = sig(&"é".repeat(300));
        assert_eq!(accented.chars().count(), SIGNATURE_CHARS);
    }

    #[test]
    fn fingerprints_are_the_start_of_the_sha_256() {
        assert_eq!(
            fingerprint("TypeError: Cannot read properties of undefined (reading 'id')"),
            "d874f3620ac13ff9"
        );
        assert_eq!(
            fingerprint(
                "ToolError: github.close_issue failed: rate limit exceeded on <n> requests"
            ),
            "ccc64b2adcd7c61d"
        );
        let fingerprint = fingerprint("x");
        assert_eq!(fingerprint.len(), FINGERPRINT_DIGITS);
        assert!(
            fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
        );
    }

    #[test]
    fn locations_are_read_from_messages_and_stacks() {
        assert_eq!(
            script_location("cannot import \"fs\" (script.ts:2:1): scripts can only import"),
            Some((2, 1))
        );
        assert_eq!(
            script_location("TypeError: x\n    at main (file:///script.ts:7:13)"),
            Some((7, 13))
        );
        assert_eq!(
            script_location(
                "Error: x\n    at f (file:///script.ts:3:9)\n    at main (file:///script.ts:12:5)"
            ),
            Some((3, 9)),
            "the innermost frame"
        );
        assert_eq!(
            script_location("script.ts:x:1, then script.ts:4:2"),
            Some((4, 2))
        );
        assert_eq!(script_location("no location"), None);
    }

    #[test]
    fn endings_come_from_failures_and_aborts() {
        let failed = RunOutcome::Failed {
            error: ScriptError {
                name: "TypeError".into(),
                message: "Cannot read properties of undefined (reading 'id') at script.ts:9:1"
                    .into(),
                stack: Some("TypeError: x\n    at main (file:///script.ts:7:13)".into()),
            },
        };
        let ending = super::ending(&failed).unwrap();
        assert_eq!(ending.error, "TypeError");
        assert_eq!(ending.line, Some(7), "the stack first");
        assert_eq!(
            ending.signature,
            "TypeError: Cannot read properties of undefined (reading 'id') at script.ts:<n>:<n>"
        );
        assert_eq!(ending.fingerprint, fingerprint(&ending.signature));

        let without_stack = RunOutcome::Failed {
            error: ScriptError {
                name: "SyntaxError".into(),
                message: "unexpected token (script.ts:4:2)".into(),
                stack: None,
            },
        };
        assert_eq!(super::ending(&without_stack).unwrap().line, Some(4));

        let aborted = RunOutcome::Aborted {
            reason: AbortReason {
                kind: AbortKind::LimitExceeded,
                message: "the script used 5000 ms of CPU time".into(),
            },
        };
        let ending = super::ending(&aborted).unwrap();
        assert_eq!(
            (ending.error.as_str(), ending.line),
            ("limit_exceeded", None)
        );
        assert_eq!(
            ending.signature,
            "limit_exceeded: the script used <n> ms of CPU time"
        );

        let completed = RunOutcome::Completed {
            output: json!(null).into(),
        };
        assert_eq!(super::ending(&completed), None);
    }
}
