//! Obtains tsgo for the target platform and embeds it in the crate.
//!
//! tsgo is the npm package `@typescript/typescript-<platform>` of TypeScript 7.0.2: the native
//! compiler (`lib/tsc`) and its lib files (`lib/lib.*.d.ts`). Its tarball is pinned by its npm
//! integrity (SHA-512), the value every npm lockfile records for it. The build downloads it from
//! the npm registry, or reads the file named by `REWIND_TSGO_TARBALL` (offline builds); either way
//! the bytes must hash to the pinned integrity. The tarball is embedded as it is, and extracted
//! to a cache directory when the checker is first used.
//!
//! A new version of tsgo changes the diagnostics workflows get: change `VERSION` and every
//! integrity together, from the registry's metadata
//! (`https://registry.npmjs.org/@typescript/typescript-<platform>/<version>`, `dist.integrity`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use sha2::{Digest as _, Sha512};

const VERSION: &str = "7.0.2";

/// Target OS and architecture as Cargo names them, npm platform, npm integrity of the tarball.
const PACKAGES: [(&str, &str, &str, &str); 4] = [
    (
        "macos",
        "aarch64",
        "darwin-arm64",
        "sha512-gowzar9MwS/aRWp6f3a4KUqzRjAZjOsmGNCM6LcTgXum+dBfgsBVMN+AgvOCCbguXyick6LJhpBszxMebJ8syA==",
    ),
    (
        "macos",
        "x86_64",
        "darwin-x64",
        "sha512-SZ9xZInqApNlNGc9s0W1VSsktYSOe9cFqNOIqmN1Gs8SmkjKZYFt017G4VwPxASInODuAdbTW7sXiFUf893RgA==",
    ),
    (
        "linux",
        "aarch64",
        "linux-arm64",
        "sha512-Qh4eU4/y3yDjnfjjyPYihMj5/ODIlmt+Bzu17OI+fiSRDW57QmU5SiN63exPRNJPKUzcc1INa1NXdrJ+MqHjUQ==",
    ),
    (
        "linux",
        "x86_64",
        "linux-x64",
        "sha512-EYdf2cNg7rgCWJnxCdJ+F3V39O8ihb37eHAu1LK8oAFizgTQbPOK7zHHXbPt8rX24COqODXeI3sIf0fCXG7H/A==",
    ),
];

/// Above any tarball of the package (about 10 MB).
const MAX_TARBALL: u64 = 64 * 1024 * 1024;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=REWIND_TSGO_TARBALL");

    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let Some((_, _, platform, integrity)) = PACKAGES
        .iter()
        .find(|(target_os, target_arch, _, _)| *target_os == os && *target_arch == arch)
    else {
        let supported: Vec<String> = PACKAGES
            .iter()
            .map(|(os, arch, _, _)| format!("{arch}-{os}"))
            .collect();
        panic!(
            "rewind-check embeds tsgo, which it has for {}, not for {arch}-{os}",
            supported.join(", ")
        );
    };

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let tarball = out_dir.join("tsgo.tgz");
    let file_name = format!("typescript-{platform}-{VERSION}.tgz");

    let bytes = match env::var_os("REWIND_TSGO_TARBALL") {
        Some(path) => {
            let path = PathBuf::from(path);
            println!("cargo:rerun-if-changed={}", path.display());
            let bytes = fs::read(&path).unwrap_or_else(|error| {
                panic!(
                    "REWIND_TSGO_TARBALL: cannot read {}: {error}",
                    path.display()
                )
            });
            check(&bytes, integrity, &path.display().to_string());
            bytes
        }
        None => match fs::read(&tarball) {
            // Built before: the tarball in OUT_DIR is the pinned one.
            Ok(bytes) if matches(&bytes, integrity) => bytes,
            _ => {
                let url = format!(
                    "https://registry.npmjs.org/@typescript/typescript-{platform}/-/{file_name}"
                );
                let bytes = download(&url, &file_name);
                check(&bytes, integrity, &url);
                bytes
            }
        },
    };
    write_if_changed(&tarball, &bytes);

    let generated = format!(
        "/// The version of TypeScript tsgo is.\n\
         pub const TSGO_VERSION: &str = {VERSION:?};\n\
         /// The npm platform of the embedded package.\n\
         pub(crate) const PLATFORM: &str = {platform:?};\n\
         /// The npm integrity of the embedded tarball.\n\
         pub(crate) const INTEGRITY: &str = {integrity:?};\n\
         /// The tarball of `@typescript/typescript-{platform}@{VERSION}`.\n\
         pub(crate) static TARBALL: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/tsgo.tgz\"));\n"
    );
    write_if_changed(&out_dir.join("tsgo.rs"), generated.as_bytes());
}

fn download(url: &str, file_name: &str) -> Vec<u8> {
    let failed = |error: &dyn std::fmt::Display| -> ! {
        panic!(
            "cannot download tsgo from {url}: {error}. For an offline build, download {file_name} \
             from the npm registry elsewhere and set REWIND_TSGO_TARBALL to its path"
        )
    };
    let mut response = ureq::get(url).call().unwrap_or_else(|error| failed(&error));
    response
        .body_mut()
        .with_config()
        .limit(MAX_TARBALL)
        .read_to_vec()
        .unwrap_or_else(|error| failed(&error))
}

fn matches(bytes: &[u8], integrity: &str) -> bool {
    let digest = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes));
    integrity.strip_prefix("sha512-") == Some(digest.as_str())
}

fn check(bytes: &[u8], integrity: &str, origin: &str) {
    assert!(
        matches(bytes, integrity),
        "the tsgo tarball from {origin} does not match its pinned integrity {integrity}"
    );
}

fn write_if_changed(path: &Path, bytes: &[u8]) {
    if fs::read(path).is_ok_and(|existing| existing == bytes) {
        return;
    }
    fs::write(path, bytes)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
}
