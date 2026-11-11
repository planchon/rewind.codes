//! Generates one test per case of the corpus: `epoch_1::blue::fan_out`, and so on.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=corpus");
    let corpus = Path::new("corpus");
    let mut tests = String::new();
    for epoch in sorted_dirs(corpus) {
        let epoch_name = name_of(&epoch);
        let _ = writeln!(tests, "mod {} {{", identifier(&epoch_name));
        for color in ["blue", "red"] {
            let _ = writeln!(tests, "    mod {color} {{");
            for case in sorted_dirs(&epoch.join(color)) {
                let case_name = name_of(&case);
                assert!(
                    case_name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "corpus case names are snake_case: {}",
                    case.display()
                );
                let _ = writeln!(
                    tests,
                    "        #[tokio::test]\n        async fn {case_name}() {{\n            \
                     ptc_e2e::check_case(\"{epoch_name}/{color}/{case_name}\").await;\n        }}"
                );
            }
            let _ = writeln!(tests, "    }}");
        }
        let _ = writeln!(tests, "}}");
    }
    let out = Path::new(&std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("corpus.rs");
    fs::write(out, tests).expect("OUT_DIR is writable");
}

fn sorted_dirs(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<_> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .expect("corpus directories have UTF-8 names")
        .to_owned()
}

fn identifier(name: &str) -> String {
    name.replace('-', "_")
}
