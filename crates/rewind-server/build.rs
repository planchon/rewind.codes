//! Embeds the dashboard's production build in the server (`archi/07-code-architecture.md`,
//! "Dashboard").
//!
//! `dashboard/dist/` is not committed. When its `index.html` is missing, or older than a source
//! of the dashboard, this script builds it with `pnpm --dir dashboard install --frozen-lockfile`
//! and `pnpm --dir dashboard build`; without pnpm it fails and says what to install.
//! `REWIND_DASHBOARD_DIST` names a build made elsewhere instead (a CI cache, a package).
//!
//! The files are embedded with `include_bytes!`, listed in `$OUT_DIR/dashboard.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use std::{env, fs};

/// The dashboard's sources and build configuration: a change of any of them builds it again.
const SOURCES: [&str; 8] = [
    "src",
    "public",
    "index.html",
    "package.json",
    "pnpm-lock.yaml",
    "vite.config.ts",
    "tsconfig.json",
    "biome.json",
];

fn main() {
    println!("cargo:rerun-if-env-changed=REWIND_DASHBOARD_DIST");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let dashboard = manifest.join("../../dashboard");
    let dist = match env::var_os("REWIND_DASHBOARD_DIST") {
        Some(dist) => {
            // A new build in the same place changes the files to embed: cargo scans the
            // directory for changes.
            let dist = PathBuf::from(dist);
            println!("cargo:rerun-if-changed={}", dist.display());
            dist
        }
        None => {
            for source in SOURCES {
                println!(
                    "cargo:rerun-if-changed={}",
                    dashboard.join(source).display()
                );
            }
            let dist = dashboard.join("dist");
            if stale(&dashboard, &dist) {
                build(&dashboard);
            }
            dist
        }
    };
    let index = dist.join("index.html");
    if !index.is_file() {
        fail(&format!(
            "{} is missing: the dashboard build is incomplete",
            index.display()
        ));
    }

    let mut files = Vec::new();
    collect(&dist, &dist, &mut files);
    files.sort();
    let mut out = String::from(
        "/// The dashboard's build: each file's path, relative to `dist/`, and its bytes.\n\
         pub(crate) static FILES: &[(&str, &[u8])] = &[\n",
    );
    for (relative, path) in &files {
        out.push_str(&format!(
            "    ({relative:?}, include_bytes!({:?})),\n",
            path.display().to_string()
        ));
    }
    out.push_str("];\n");
    let target = PathBuf::from(env::var("OUT_DIR").expect("set by cargo")).join("dashboard.rs");
    fs::write(&target, out).expect("OUT_DIR is writable");
}

/// Whether `dist/index.html` is missing or older than a source.
fn stale(dashboard: &Path, dist: &Path) -> bool {
    let Some(built) = modified(&dist.join("index.html")) else {
        return true;
    };
    SOURCES
        .iter()
        .filter_map(|source| newest(&dashboard.join(source)))
        .any(|source| source > built)
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// The newest modification time under `path`.
fn newest(path: &Path) -> Option<SystemTime> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_dir() {
        return meta.modified().ok();
    }
    fs::read_dir(path)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| newest(&entry.path()))
        .max()
}

fn build(dashboard: &Path) {
    let pnpm = env::var_os("PNPM").map_or_else(|| PathBuf::from("pnpm"), PathBuf::from);
    for args in [
        &["--dir", "", "install", "--frozen-lockfile"][..],
        &["--dir", "", "build"][..],
    ] {
        let mut command = Command::new(&pnpm);
        for arg in args {
            if arg.is_empty() {
                command.arg(dashboard);
            } else {
                command.arg(arg);
            }
        }
        match command.status() {
            Ok(status) if status.success() => {}
            Ok(status) => fail(&format!(
                "`{} {}` failed ({status})",
                pnpm.display(),
                args.join(" ").replace("--dir  ", "--dir dashboard ")
            )),
            Err(error) => fail(&format!(
                "cannot run pnpm ({error}). The server embeds the dashboard's build: install \
                 Node 22.12 or later and pnpm 10 (`corepack enable`), or set REWIND_DASHBOARD_DIST \
                 to a dashboard built with `pnpm --dir dashboard build`"
            )),
        }
    }
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|error| {
        fail(&format!("cannot read {}: {error}", dir.display()));
    });
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files);
        } else if let Ok(relative) = path.strip_prefix(root) {
            let relative = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let absolute = fs::canonicalize(&path).unwrap_or(path);
            files.push((relative, absolute));
        }
    }
}

fn fail(message: &str) -> ! {
    eprintln!("error: rewind-server embeds the dashboard: {message}");
    std::process::exit(1);
}
