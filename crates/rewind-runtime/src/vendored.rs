//! The JavaScript of `deno_web` and `deno_webidl`, embedded in the binary (`vendor/`).
//!
//! deno_core records an extension's sources by their path on the build machine
//! (`LoadedFromFsDuringSnapshot`) and reads them when a snapshot is built, expecting a build
//! script to build it. This crate builds its snapshots at run time, on whatever machine runs the
//! binary, where that path does not exist: the snapshot build gets these copies from memory
//! instead (`LoadedFromMemoryDuringSnapshot`). Isolates started from the snapshot never read an
//! extension's sources.
//!
//! The two crates are pinned exactly and their JavaScript is part of the determinism epoch; a
//! test checks that each copy is the file its crate records.

use std::borrow::Cow;

use deno_core::ascii_str_include;
use deno_core::{Extension, ExtensionFileSource, ExtensionFileSourceCode, FastStaticString};
use rewind_engine::ports::ExecutorError;

/// The embedded copy of an extension's file, by its specifier.
fn source(specifier: &str) -> Option<FastStaticString> {
    Some(match specifier {
        "ext:deno_webidl/00_webidl.js" => {
            ascii_str_include!("../vendor/deno_webidl/00_webidl.js")
        }
        "ext:deno_web/00_infra.js" => ascii_str_include!("../vendor/deno_web/00_infra.js"),
        "ext:deno_web/00_url.js" => ascii_str_include!("../vendor/deno_web/00_url.js"),
        "ext:deno_web/01_broadcast_channel.js" => {
            ascii_str_include!("../vendor/deno_web/01_broadcast_channel.js")
        }
        "ext:deno_web/01_console.js" => ascii_str_include!("../vendor/deno_web/01_console.js"),
        "ext:deno_web/01_dom_exception.js" => {
            ascii_str_include!("../vendor/deno_web/01_dom_exception.js")
        }
        "ext:deno_web/01_mimesniff.js" => {
            ascii_str_include!("../vendor/deno_web/01_mimesniff.js")
        }
        "ext:deno_web/01_urlpattern.js" => {
            ascii_str_include!("../vendor/deno_web/01_urlpattern.js")
        }
        "ext:deno_web/02_event.js" => ascii_str_include!("../vendor/deno_web/02_event.js"),
        "ext:deno_web/02_structured_clone.js" => {
            ascii_str_include!("../vendor/deno_web/02_structured_clone.js")
        }
        "ext:deno_web/02_timers.js" => ascii_str_include!("../vendor/deno_web/02_timers.js"),
        "ext:deno_web/03_abort_signal.js" => {
            ascii_str_include!("../vendor/deno_web/03_abort_signal.js")
        }
        "ext:deno_web/04_global_interfaces.js" => {
            ascii_str_include!("../vendor/deno_web/04_global_interfaces.js")
        }
        "ext:deno_web/05_base64.js" => ascii_str_include!("../vendor/deno_web/05_base64.js"),
        "ext:deno_web/06_streams.js" => ascii_str_include!("../vendor/deno_web/06_streams.js"),
        "ext:deno_web/08_text_encoding.js" => {
            ascii_str_include!("../vendor/deno_web/08_text_encoding.js")
        }
        "ext:deno_web/09_file.js" => ascii_str_include!("../vendor/deno_web/09_file.js"),
        "ext:deno_web/10_filereader.js" => {
            ascii_str_include!("../vendor/deno_web/10_filereader.js")
        }
        "ext:deno_web/12_location.js" => ascii_str_include!("../vendor/deno_web/12_location.js"),
        "ext:deno_web/13_message_port.js" => {
            ascii_str_include!("../vendor/deno_web/13_message_port.js")
        }
        "ext:deno_web/14_compression.js" => {
            ascii_str_include!("../vendor/deno_web/14_compression.js")
        }
        "ext:deno_web/15_performance.js" => {
            ascii_str_include!("../vendor/deno_web/15_performance.js")
        }
        "ext:deno_web/16_image_data.js" => {
            ascii_str_include!("../vendor/deno_web/16_image_data.js")
        }
        "ext:deno_web/17_geometry.js" => ascii_str_include!("../vendor/deno_web/17_geometry.js"),
        "ext:deno_web/18_css_stylesheet.js" => {
            ascii_str_include!("../vendor/deno_web/18_css_stylesheet.js")
        }
        "ext:deno_web/locks.js" => ascii_str_include!("../vendor/deno_web/locks.js"),
        "ext:deno_web/webtransport.js" => ascii_str_include!("../vendor/deno_web/webtransport.js"),
        _ => return None,
    })
}

/// The extensions with every source their crate reads from the disk replaced by its embedded
/// copy: what the snapshot build is given.
pub(crate) fn for_snapshot(
    mut extensions: Vec<Extension>,
) -> Result<Vec<Extension>, ExecutorError> {
    for extension in &mut extensions {
        for files in [
            &mut extension.js_files,
            &mut extension.esm_files,
            &mut extension.lazy_loaded_js_files,
            &mut extension.lazy_loaded_esm_files,
        ] {
            let embedded = files
                .iter()
                .map(|file| match file.code {
                    ExtensionFileSourceCode::LoadedFromFsDuringSnapshot(_) => {
                        source(file.specifier)
                            .map(|code| {
                                ExtensionFileSource::loaded_from_memory_during_snapshot(
                                    file.specifier,
                                    code,
                                )
                            })
                            .ok_or_else(|| {
                                ExecutorError::Internal(format!(
                                    "no embedded copy of {}: add it to rewind-runtime's vendor/",
                                    file.specifier
                                ))
                            })
                    }
                    _ => Ok(file.clone()),
                })
                .collect::<Result<Vec<_>, _>>()?;
            *files = Cow::Owned(embedded);
        }
    }
    Ok(extensions)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::ops;

    /// Each file the extensions read from the disk, with the path their crate records.
    fn recorded() -> Vec<(&'static str, &'static str)> {
        let mut files = Vec::new();
        for extension in ops::extensions() {
            for file in extension
                .js_files
                .iter()
                .chain(extension.esm_files.iter())
                .chain(extension.lazy_loaded_js_files.iter())
                .chain(extension.lazy_loaded_esm_files.iter())
            {
                if let ExtensionFileSourceCode::LoadedFromFsDuringSnapshot(path) = file.code {
                    files.push((file.specifier, path));
                }
            }
        }
        files
    }

    #[test]
    fn the_snapshot_build_reads_nothing_from_the_disk() {
        assert_eq!(recorded().len(), 27, "deno_webidl's file and deno_web's 26");
        let extensions = for_snapshot(ops::extensions()).unwrap_or_else(|error| panic!("{error}"));
        for extension in &extensions {
            for file in extension
                .js_files
                .iter()
                .chain(extension.esm_files.iter())
                .chain(extension.lazy_loaded_js_files.iter())
                .chain(extension.lazy_loaded_esm_files.iter())
            {
                assert!(
                    !matches!(
                        file.code,
                        ExtensionFileSourceCode::LoadedFromFsDuringSnapshot(_)
                    ),
                    "{} is still read from the disk",
                    file.specifier
                );
            }
        }
    }

    /// The copies are the files of the pinned crates. `REWIND_RUNTIME_VENDOR=1` copies them again.
    #[test]
    fn the_vendored_copies_are_the_files_of_the_pinned_crates() {
        let vendor = Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor");
        let write = std::env::var_os("REWIND_RUNTIME_VENDOR").is_some();
        for (specifier, path) in recorded() {
            let Ok(original) = std::fs::read_to_string(path) else {
                eprintln!("SKIPPED: {path} is not on this machine; {specifier} is not compared");
                continue;
            };
            let relative = specifier
                .strip_prefix("ext:")
                .unwrap_or_else(|| panic!("{specifier} is not an extension's specifier"));
            if write {
                let target = vendor.join(relative);
                std::fs::create_dir_all(target.parent().unwrap_or(&vendor))
                    .unwrap_or_else(|error| panic!("{error}"));
                std::fs::write(&target, &original).unwrap_or_else(|error| panic!("{error}"));
                continue;
            }
            let embedded = source(specifier).unwrap_or_else(|| {
                panic!("no embedded copy of {specifier}: add vendor/{relative} to `source`")
            });
            assert!(
                embedded.as_str() == original,
                "vendor/{relative} differs from {path}: run REWIND_RUNTIME_VENDOR=1 cargo test -p \
                 rewind-runtime vendored"
            );
        }
    }
}
