//! `unsafe` code lives in one module of the workspace, `rewind-runtime`'s `memory`
//! (archi/07-code-architecture.md, "Dependency rules"): what V8 offers only through raw pointers.
//! The workspace denies `unsafe_code`, every crate takes the workspace's lints, and no source
//! file but that module uses `unsafe` or relaxes the lint.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

/// The one file allowed to hold `unsafe` code, relative to the workspace.
const UNSAFE_MODULE: &str = "crates/rewind-runtime/src/memory.rs";
/// The crate root that denies `unsafe_code` instead of forbidding it, so that its module may
/// allow it.
const DENYING_ROOT: &str = "crates/rewind-runtime/src/lib.rs";
/// This test, which spells what it looks for.
const THIS_TEST: &str = "crates/rewind-architecture-tests/tests/unsafe_code.rs";

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn members(root: &Path) -> Vec<PathBuf> {
    let mut members: Vec<PathBuf> = fs::read_dir(root.join("crates"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("Cargo.toml").is_file())
        .collect();
    members.push(root.join("evals"));
    members.sort();
    members
}

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if path.is_dir() {
            if name != "target" && name != "node_modules" && !name.starts_with('.') {
                rust_files(&path, files);
            }
        } else if name.ends_with(".rs") {
            files.push(path);
        }
    }
}

/// The lines of `source` that use `unsafe`, or name the `unsafe_code` lint other than to forbid
/// it (or to deny it, when `may_deny`). Comments are skipped.
fn unsafe_lines(source: &str, may_deny: bool) -> Vec<(usize, String)> {
    const KEYWORDS: [&str; 6] = [
        "unsafe {",
        "unsafe fn",
        "unsafe extern",
        "unsafe impl",
        "unsafe trait",
        "unsafe(",
    ];
    source
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let code = line.split("//").next().unwrap_or_default();
            let keyword = KEYWORDS.iter().any(|keyword| code.contains(keyword));
            let lint = code.contains("unsafe_code")
                && !code.contains("forbid(unsafe_code)")
                && !(may_deny && code.contains("deny(unsafe_code)"));
            (keyword || lint).then(|| (index + 1, line.trim().to_owned()))
        })
        .collect()
}

#[test]
fn the_workspace_denies_unsafe_code_and_every_crate_takes_its_lints() {
    let root = workspace();
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let lints = manifest
        .split("[workspace.lints.rust]")
        .nth(1)
        .expect("the workspace declares its Rust lints");
    let lints = lints.split("\n[").next().unwrap();
    assert!(
        lints.contains("unsafe_code = \"deny\""),
        "the workspace must deny unsafe_code"
    );

    for member in members(&root) {
        let manifest = fs::read_to_string(member.join("Cargo.toml")).unwrap();
        let lints = manifest
            .split("\n[lints]")
            .nth(1)
            .map(|lints| lints.split("\n[").next().unwrap().to_owned())
            .unwrap_or_default();
        assert!(
            lints.contains("workspace = true"),
            "{} must take the workspace's lints",
            member.display()
        );
        assert!(
            !manifest.contains("unsafe_code"),
            "{} must not set unsafe_code itself",
            member.display()
        );
    }
}

#[test]
fn only_the_memory_module_of_the_runtime_uses_unsafe() {
    let root = workspace();
    let mut files = Vec::new();
    for member in members(&root) {
        rust_files(&member, &mut files);
    }
    let mut violations = Vec::new();
    for file in &files {
        let relative = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if relative == UNSAFE_MODULE || relative == THIS_TEST {
            continue;
        }
        let source = fs::read_to_string(file).unwrap();
        for (line, text) in unsafe_lines(&source, relative == DENYING_ROOT) {
            violations.push(format!("{relative}:{line}: {text}"));
        }
    }
    assert!(
        violations.is_empty(),
        "unsafe code outside {UNSAFE_MODULE}:\n{}",
        violations.join("\n")
    );

    let runtime = fs::read_to_string(root.join(DENYING_ROOT)).unwrap();
    assert!(runtime.contains("#![deny(unsafe_code)]"));
    let memory = fs::read_to_string(root.join(UNSAFE_MODULE)).unwrap();
    assert!(memory.contains("#![allow(unsafe_code)]"));
    assert!(memory.contains("#![deny(clippy::undocumented_unsafe_blocks"));
}

#[test]
fn the_scan_finds_unsafe_code_and_skips_comments() {
    let found = unsafe_lines(
        "#![forbid(unsafe_code)]\n\
         // unsafe { comment }\n\
         let x = unsafe { f() };\n\
         #![allow(unsafe_code)]\n\
         #[unsafe(no_mangle)]\n\
         #![deny(unsafe_code)]\n\
         SourcePos::unsafely_from_byte_pos(0)\n",
        false,
    );
    let lines: Vec<usize> = found.iter().map(|(line, _)| *line).collect();
    assert_eq!(lines, [3, 4, 5, 6]);
    assert_eq!(unsafe_lines("#![deny(unsafe_code)]", true), []);
}
