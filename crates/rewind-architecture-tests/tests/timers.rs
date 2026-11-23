//! Nothing polls (`archi/07-code-architecture.md`, principle 11 and "Dependency rules"): a
//! component waits on one channel and wakes when a message arrives. `tokio::time::interval`,
//! `tokio::time::sleep`, `sleep_until` and `std::thread::sleep` appear in the workspace's sources
//! only where the spec has a timer: the engine's timer wheel, the backoff of a retry after an
//! error, and the clocks that read no store. [`EXCEPTIONS`] names each function that holds one,
//! and why; a use anywhere else fails, with its file and line.
//!
//! Tests (`tests/`, `#[cfg(test)]` items and the modules they declare) and `examples/` are left
//! out. The timers of the dependencies stay in their crates: SlateDB's, hyper's and axum's (the
//! SSE keep-alive comment), OpenTelemetry's periodic export of metrics. The dashboard's lint
//! refuses `refetchInterval` on its side (`dashboard/lint/no-polling.grit`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// What a timer is there for, as principle 11 allows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// The timer wheel of a partition: the one place a node sleeps.
    Wheel,
    /// The wait before a retry, after an error.
    Backoff,
    /// A clock that reads no store: it bounds a wait, or wakes what reads nothing on a timer.
    Clock,
}

/// A function of the workspace allowed its timers: its file, its name, what for, and why.
struct Exception {
    file: &'static str,
    function: &'static str,
    kind: Kind,
    reason: &'static str,
}

/// Every function of the workspace's sources that holds a timer. A use in another function
/// fails; an entry whose function holds no timer fails too, so that the list stays
/// the list of what is there.
const EXCEPTIONS: &[Exception] = &[
    Exception {
        file: "crates/rewind-engine/src/partition/wheel.rs",
        function: "until",
        kind: Kind::Wheel,
        reason: "the timer wheel: the node's task waits until the earliest timer of its wheels",
    },
    Exception {
        file: "crates/rewind-engine/src/node/runtime.rs",
        function: "watch_membership",
        kind: Kind::Backoff,
        reason: "a watch of the cluster's next node, or of the control plane's changes, that \
                 failed is tried again after the engine's backoff",
    },
    Exception {
        file: "crates/rewind-server/src/idle.rs",
        function: "until_idle",
        kind: Kind::Clock,
        reason: "the embedded server's idle stop: one timer, pushed back by each request and each \
                 activation",
    },
    Exception {
        file: "crates/rewind-server/src/app.rs",
        function: "wait_for_end",
        kind: Kind::Clock,
        reason: "the deadline of a held request (`wait`): the run's changes wake it, the deadline \
                 ends it",
    },
    Exception {
        file: "crates/rewind-server/src/streams.rs",
        function: "lost",
        kind: Kind::Clock,
        reason: "the expiry of the token that opened a stream, which closes it; rights are read \
                 again on a change of the control plane, never on this timer",
    },
    Exception {
        file: "crates/rewind-server/src/wiring/jobs.rs",
        function: "daily",
        kind: Kind::Clock,
        reason: "the control plane's daily jobs (tokens and connections to delete, tests no \
                 version references): they read the store once a day, nothing between",
    },
    Exception {
        file: "crates/rewind-server/src/serve.rs",
        function: "control_plane",
        kind: Kind::Backoff,
        reason: "the control plane's change stream that cannot be listened to again is tried \
                 after a second",
    },
    Exception {
        file: "crates/rewind-openapi/src/dispatch.rs",
        function: "honor_retry_after",
        kind: Kind::Backoff,
        reason: "a transient failure's `Retry-After`, waited out within the execution timeout",
    },
    Exception {
        file: "crates/rewind-cli/src/embedded.rs",
        function: "wait_for_server",
        kind: Kind::Backoff,
        reason: "the embedded server's socket refused the CLI while another command's server \
                 starts: connected again after a doubling backoff, up to the start timeout",
    },
    Exception {
        file: "evals/src/model/mod.rs",
        function: "post_json",
        kind: Kind::Backoff,
        reason: "a model API that failed or limited the rate is called again after its \
                 `retry-after` or a doubling backoff",
    },
    Exception {
        file: "crates/rewind-load/src/load.rs",
        function: "wait_until_ready",
        kind: Kind::Backoff,
        reason: "the load generator asks for the server's info again after a refused connection, \
                 until the server is up",
    },
    Exception {
        file: "crates/rewind-load/src/load.rs",
        function: "host_worker",
        kind: Kind::Backoff,
        reason: "the load generator's host worker opens its task stream again after a drop",
    },
    Exception {
        file: "crates/rewind-load/src/load.rs",
        function: "drive",
        kind: Kind::Backoff,
        reason: "a start lost in transit is sent again with its idempotency key",
    },
    Exception {
        file: "crates/rewind-load/src/load.rs",
        function: "wait_for_end",
        kind: Kind::Backoff,
        reason: "a held read of a run that failed is sent again; the wait itself is held by the \
                 server",
    },
];

/// The paths the guard reads: every member's `.rs` files but its tests and examples.
fn sources(root: &Path) -> Vec<PathBuf> {
    let mut members: Vec<PathBuf> = fs::read_dir(root.join("crates"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("Cargo.toml").is_file())
        .collect();
    members.push(root.join("evals"));
    members.sort();
    let mut files = Vec::new();
    for member in members {
        rust_files(&member, &mut files);
    }
    files.sort();
    files
}

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if path.is_dir() {
            let skipped = ["target", "node_modules", "tests", "examples", "benches"];
            if !skipped.contains(&name.as_ref()) && !name.starts_with('.') {
                rust_files(&path, files);
            }
        } else if name.ends_with(".rs") {
            files.push(path);
        }
    }
}

/// `source` with its comments and the contents of its string and character literals blanked,
/// lines kept: what the guard reads, so that a doc comment or a script in a string never counts.
fn code_of(source: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        Line,
        Block(u32),
        Str,
        Raw(usize),
    }
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut state = State::Code;
    let mut i = 0;
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match state {
            State::Code => {
                let ident_before = i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
                if c == '/' && next == Some('/') {
                    state = State::Line;
                    out.push(' ');
                } else if c == '/' && next == Some('*') {
                    state = State::Block(1);
                    out.push_str("  ");
                    i += 1;
                } else if c == '"' {
                    state = State::Str;
                    out.push('"');
                } else if (c == 'r' || (c == 'b' && next == Some('r'))) && !ident_before {
                    // A raw string: `r"…"`, `r#"…"#`, `br"…"`.
                    let start = if c == 'b' { i + 2 } else { i + 1 };
                    let hashes = chars[start..].iter().take_while(|&&h| h == '#').count();
                    if chars.get(start + hashes) == Some(&'"') {
                        out.extend(std::iter::repeat_n(' ', start + hashes - i));
                        out.push('"');
                        i = start + hashes + 1;
                        state = State::Raw(hashes);
                        continue;
                    }
                    out.push(c);
                } else if c == '\'' {
                    // A character literal (`'{'`, `'\n'`), or a lifetime (`'a`), which stays.
                    let escaped = next == Some('\\');
                    let close = if escaped {
                        chars[i + 2..]
                            .iter()
                            .position(|&q| q == '\'')
                            .map(|at| i + 2 + at)
                    } else {
                        (chars.get(i + 2) == Some(&'\'')).then_some(i + 2)
                    };
                    match close {
                        Some(end) => {
                            out.push('\'');
                            out.extend(std::iter::repeat_n(' ', end - i - 1));
                            out.push('\'');
                            i = end + 1;
                            continue;
                        }
                        None => out.push(c),
                    }
                } else {
                    out.push(c);
                }
            }
            State::Line => {
                if c == '\n' {
                    state = State::Code;
                }
                out.push(blank(c));
            }
            State::Block(depth) => {
                if c == '*' && next == Some('/') {
                    state = if depth == 1 {
                        State::Code
                    } else {
                        State::Block(depth - 1)
                    };
                    out.push_str("  ");
                    i += 1;
                } else if c == '/' && next == Some('*') {
                    state = State::Block(depth + 1);
                    out.push_str("  ");
                    i += 1;
                } else {
                    out.push(blank(c));
                }
            }
            State::Str => {
                if c == '\\' {
                    out.push(' ');
                    if let Some(escaped) = next {
                        out.push(blank(escaped));
                    }
                    i += 1;
                } else if c == '"' {
                    state = State::Code;
                    out.push('"');
                } else {
                    out.push(blank(c));
                }
            }
            State::Raw(hashes) => {
                let closes = c == '"'
                    && chars[i + 1..]
                        .iter()
                        .take(hashes)
                        .filter(|&&h| h == '#')
                        .count()
                        == hashes;
                if closes {
                    out.push('"');
                    out.extend(std::iter::repeat_n(' ', hashes));
                    i += 1 + hashes;
                    state = State::Code;
                    continue;
                }
                out.push(blank(c));
            }
        }
        i += 1;
    }
    out
}

/// `code` without the items under `#[cfg(test)]` (or `#[cfg(all(test, …))]`): each blanked from
/// its attribute to the end of its body, or of its declaration (`mod tests;`). Returns the names
/// of the modules declared that way, whose files are tests too.
fn without_tests(code: &str) -> (String, Vec<String>) {
    let lines: Vec<&str> = code.lines().collect();
    let mut kept: Vec<String> = Vec::with_capacity(lines.len());
    let mut declared = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim();
        let is_test = line.starts_with("#[cfg(test)]") || line.starts_with("#[cfg(all(test");
        if !is_test {
            kept.push(lines[index].to_owned());
            index += 1;
            continue;
        }
        // The item the attribute applies to: its other attributes, then its lines up to the end
        // of its body or the `;` that ends it.
        let mut depth = 0i64;
        let mut opened = false;
        loop {
            let text = lines[index];
            kept.push(String::new());
            let trimmed = text.trim();
            if let Some(name) = trimmed
                .strip_prefix("mod ")
                .or_else(|| trimmed.strip_prefix("pub mod "))
                .or_else(|| trimmed.strip_prefix("pub(crate) mod "))
                .and_then(|rest| rest.strip_suffix(';'))
            {
                declared.push(name.trim().to_owned());
            }
            for ch in text.chars() {
                match ch {
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            index += 1;
            let attribute = trimmed.starts_with("#[");
            // A body closed, or an item without one: `mod tests;`, a field, an argument.
            let ended = (opened && depth <= 0)
                || (!opened && !attribute && (trimmed.ends_with(';') || trimmed.ends_with(',')));
            if ended || index >= lines.len() {
                break;
            }
        }
    }
    (kept.join("\n"), declared)
}

/// The names a timer goes by: `time::sleep`, or the name imported from `tokio::time` or
/// `std::thread`.
const TIMERS: [&str; 4] = ["sleep", "sleep_until", "interval", "interval_at"];

/// A timer used in the code: its line (from 1), the function it is in, the line itself.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Use {
    line: usize,
    function: String,
    text: String,
}

/// The words of a line of code, with where each starts.
fn words(line: &str) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    let mut start = None;
    for (at, ch) in line.char_indices() {
        let word = ch.is_alphanumeric() || ch == '_';
        match (word, start) {
            (true, None) => start = Some(at),
            (false, Some(from)) => {
                found.push((from, &line[from..at]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        found.push((from, &line[from..]));
    }
    found
}

/// The names `code` imports from `tokio::time` or `std::thread`, which a bare call then uses.
fn imported_timers(code: &str) -> BTreeSet<String> {
    let mut imported = BTreeSet::new();
    let mut statement: Option<String> = None;
    for line in code.lines() {
        let trimmed = line.trim();
        if statement.is_none() && (trimmed.starts_with("use ") || trimmed.starts_with("pub use ")) {
            statement = Some(String::new());
        }
        let Some(text) = statement.as_mut() else {
            continue;
        };
        text.push_str(trimmed);
        text.push(' ');
        if trimmed.ends_with(';') {
            if text.contains("tokio::time::") || text.contains("std::thread::") {
                for (_, word) in words(text) {
                    if TIMERS.contains(&word) {
                        imported.insert(word.to_owned());
                    }
                }
            }
            statement = None;
        }
    }
    imported
}

/// The name a line declares with `fn`, if it declares one (a function pointer type `fn(…)` is
/// not a declaration).
fn declared_function(line: &str) -> Option<String> {
    let found = words(line);
    found.iter().find_map(|(at, word)| {
        if *word != "fn" {
            return None;
        }
        let rest = &line[at + 2..];
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let name: String = rest
            .trim_start()
            .chars()
            .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
            .collect();
        (!name.is_empty()).then_some(name)
    })
}

/// The timers of `source`, tests left out.
fn timers(source: &str) -> (Vec<Use>, Vec<String>) {
    let (code, declared) = without_tests(&code_of(source));
    let original: Vec<&str> = source.lines().collect();
    let imported = imported_timers(&code);
    let mut uses = Vec::new();
    let mut function = String::from("(outside a function)");
    for (index, line) in code.lines().enumerate() {
        if let Some(name) = declared_function(line) {
            function = name;
        }
        let used = words(line).iter().any(|(at, word)| {
            if !TIMERS.contains(word) {
                return false;
            }
            let before = line[..*at].trim_end();
            // `tokio::time::sleep`, `std::thread::sleep`, `time::sleep_until`…
            let path = before.ends_with("time::") || before.ends_with("thread::");
            // A bare call of a name imported from them: not a method, not a declaration.
            let call = line[at + word.len()..].trim_start().starts_with('(')
                && !before.ends_with('.')
                && !before.ends_with("::")
                && !before.ends_with("fn")
                && imported.contains(*word);
            path || call
        });
        if used {
            uses.push(Use {
                line: index + 1,
                function: function.clone(),
                text: original
                    .get(index)
                    .map_or("", |text| text.trim())
                    .to_owned(),
            });
        }
    }
    (uses, declared)
}

/// The files of the modules declared under `#[cfg(test)]` by `file`.
fn test_modules(file: &Path, declared: &[String]) -> Vec<PathBuf> {
    let name = file.file_stem().unwrap().to_string_lossy();
    let dir = file.parent().unwrap();
    let base = if ["mod", "lib", "main"].contains(&name.as_ref()) {
        dir.to_path_buf()
    } else {
        dir.join(name.as_ref())
    };
    declared
        .iter()
        .flat_map(|module| [base.join(format!("{module}.rs")), base.join(module)])
        .collect()
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn relative(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn nothing_polls_timers_are_only_where_the_spec_has_them() {
    let root = workspace();
    let files = sources(&root);
    let mut found: BTreeMap<String, Vec<Use>> = BTreeMap::new();
    let mut tests: Vec<PathBuf> = Vec::new();
    for file in &files {
        let (uses, declared) = timers(&fs::read_to_string(file).unwrap());
        tests.extend(test_modules(file, &declared));
        if !uses.is_empty() {
            found.insert(relative(&root, file), uses);
        }
    }
    // A file of a test module, or under one, is a test.
    let tests: Vec<String> = tests.iter().map(|path| relative(&root, path)).collect();
    found.retain(|file, _| {
        !tests
            .iter()
            .any(|test| file == test || file.starts_with(&format!("{test}/")))
    });

    let mut violations = Vec::new();
    for (file, uses) in &found {
        for use_ in uses {
            let allowed = EXCEPTIONS
                .iter()
                .any(|exception| exception.file == file && exception.function == use_.function);
            if !allowed {
                violations.push(format!(
                    "{file}:{}: {} (in fn {})",
                    use_.line, use_.text, use_.function
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "a timer outside the timer wheel, the backoff helpers and the clocks that read no store \
         (archi/07-code-architecture.md, principle 11): wait on a channel instead, or, for a \
         backoff after an error or a clock that reads no store, add the function to EXCEPTIONS \
         with its reason:\n{}",
        violations.join("\n")
    );

    let stale: Vec<String> = EXCEPTIONS
        .iter()
        .filter(|exception| {
            !found
                .get(exception.file)
                .is_some_and(|uses| uses.iter().any(|use_| use_.function == exception.function))
        })
        .map(|exception| format!("{} fn {}", exception.file, exception.function))
        .collect();
    assert!(
        stale.is_empty(),
        "exceptions whose function holds no timer: remove them from EXCEPTIONS:\n{}",
        stale.join("\n")
    );
}

#[test]
fn every_exception_says_why() {
    for exception in EXCEPTIONS {
        assert!(
            !exception.reason.trim().is_empty(),
            "{} fn {}: an exception gives its reason",
            exception.file,
            exception.function
        );
    }
    assert!(
        EXCEPTIONS
            .iter()
            .filter(|exception| exception.kind == Kind::Wheel)
            .count()
            == 1,
        "the timer wheel is the one place a node sleeps"
    );
}

#[test]
fn the_scan_finds_timers_and_skips_comments_strings_and_tests() {
    let source = r##"
use tokio::time::{Instant, sleep};
// tokio::time::sleep(d) in a comment
/// Waits: `tokio::time::interval(d)` in a doc comment.
fn first() {
    let script = "await sleep(\"1h\"); tokio::time::sleep(x)";
    let raw = r#"std::thread::sleep(d) { "#;
    let brace = '{';
    tokio::time::sleep(d).await;
}
fn second<'a>(x: &'a str) {
    sleep(d).await;
    let mut ticks = tokio::time::interval(d);
    std::thread::sleep(d);
    time::sleep_until(at).await;
    command.sleep_for(d);
    worker.sleep(d);
    Sleep::new();
    let f: fn(u32) -> u32 = id;
}
#[cfg(test)]
mod tests {
    fn helper() {
        tokio::time::sleep(d);
    }
}
#[cfg(test)]
mod more;
fn third() {
    tokio::time::sleep(d);
}
"##;
    let (uses, declared) = timers(source);
    let found: Vec<(usize, &str)> = uses
        .iter()
        .map(|use_| (use_.line, use_.function.as_str()))
        .collect();
    assert_eq!(
        found,
        [
            (9, "first"),
            (12, "second"),
            (13, "second"),
            (14, "second"),
            (15, "second"),
            (30, "third"),
        ]
    );
    assert_eq!(declared, ["more"]);
    assert_eq!(
        test_modules(Path::new("crates/x/src/http/mod.rs"), &declared),
        [
            PathBuf::from("crates/x/src/http/more.rs"),
            PathBuf::from("crates/x/src/http/more")
        ]
    );
    assert_eq!(
        test_modules(Path::new("crates/x/src/wiring.rs"), &declared)[0],
        PathBuf::from("crates/x/src/wiring/more.rs")
    );
}
