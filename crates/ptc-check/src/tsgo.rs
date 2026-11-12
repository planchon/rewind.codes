//! tsgo, the native TypeScript 7 compiler, embedded and driven as a CLI: TypeScript 7.0 has no
//! programmatic API (`archi/04-tools.md`, "Type-checking").
//!
//! The crate embeds the npm package `@typescript/typescript-<platform>` as its tarball, pinned
//! by its npm integrity (see `build.rs`). The first use extracts it to
//! `<cache>/typescript-<version>-<platform>/`: into a staging directory, renamed at the end, so
//! the directory is complete whenever it exists, and processes extracting at the same time
//! leave one copy.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use flate2::read::GzDecoder;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::CheckError;

include!(concat!(env!("OUT_DIR"), "/tsgo.rs"));

/// Written last in the extracted directory: the integrity of the tarball it came from.
const MARKER: &str = ".integrity";

/// The executable, in the extracted directory.
const EXECUTABLE: &str = "lib/tsc";

/// What is read of tsgo's standard output at most: well above the diagnostics a check returns
/// ([`TsgoChecker::MAX_DIAGNOSTICS`](crate::TsgoChecker::MAX_DIAGNOSTICS)). Past it, tsgo is
/// stopped and its last line dropped: a script of 2 MiB with an error on each line made tsgo
/// print 700,000 diagnostics.
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

/// What is kept of tsgo's standard error, for the error of a check that failed.
const MAX_ERRORS: usize = 64 * 1024;

/// Extracts tsgo into `cache_dir` unless it is there already, and returns the executable.
pub(crate) fn install(cache_dir: &Path) -> Result<PathBuf, CheckError> {
    let target = cache_dir.join(format!("typescript-{TSGO_VERSION}-{PLATFORM}"));
    let executable = target.join(EXECUTABLE);
    if is_complete(&target) {
        return Ok(executable);
    }
    let failed = |source: io::Error| CheckError::Install {
        dir: cache_dir.to_owned(),
        source,
    };
    fs::create_dir_all(cache_dir).map_err(failed)?;
    let staging = tempfile::Builder::new()
        .prefix(".typescript-")
        .tempdir_in(cache_dir)
        .map_err(failed)?;
    unpack(staging.path()).map_err(failed)?;
    fs::write(staging.path().join(MARKER), INTEGRITY).map_err(failed)?;
    if fs::rename(staging.path(), &target).is_err() {
        // Another process renamed its copy first: the staging directory is deleted.
        if is_complete(&target) {
            return Ok(executable);
        }
        // A copy that does not match the tarball, damaged or partly deleted: moved aside
        // (and deleted with the directory it is moved to), then replaced.
        let stale = tempfile::Builder::new()
            .prefix(".typescript-stale-")
            .tempdir_in(cache_dir)
            .map_err(failed)?;
        let _ = fs::rename(&target, stale.path().join("typescript"));
        if let Err(error) = fs::rename(staging.path(), &target) {
            return if is_complete(&target) {
                Ok(executable)
            } else {
                Err(failed(error))
            };
        }
    }
    // Renamed: nothing is left to delete.
    let _ = staging.keep();
    tracing::info!(dir = %target.display(), "extracted tsgo {TSGO_VERSION}");
    Ok(executable)
}

fn is_complete(dir: &Path) -> bool {
    fs::read_to_string(dir.join(MARKER)).is_ok_and(|integrity| integrity == INTEGRITY)
        && dir.join(EXECUTABLE).is_file()
}

/// Writes the package's files (`package/…` in the tarball) into `dir`.
fn unpack(dir: &Path) -> io::Result<()> {
    let mut archive = tar::Archive::new(GzDecoder::new(TARBALL));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.into_owned();
        let Ok(relative) = path.strip_prefix("package") else {
            continue;
        };
        if !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected path in the tsgo tarball: {}", path.display()),
            ));
        }
        let destination = dir.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        // Keeps the mode of the header: the executable stays executable.
        entry.unpack(&destination)?;
    }
    Ok(())
}

/// What one run of tsgo printed.
#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) diagnostics: Vec<Reported>,
}

/// A diagnostic as tsgo prints it with `--pretty false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reported {
    /// The file, relative to the project's directory, with its 1-based line and column (in
    /// UTF-16 code units). `None` for a diagnostic about the configuration.
    pub(crate) location: Option<(String, u32, u32)>,
    pub(crate) warning: bool,
    pub(crate) code: u32,
    /// With the lines of a chained message, indented as tsgo indents them.
    pub(crate) message: String,
}

/// Type-checks the project in `dir`, as `tsc --project dir`.
///
/// - `--noResolve`: nothing outside `dir` is read. An import or a reference of the script
///   could otherwise make tsgo read any file of the host, and its diagnostics show what they
///   read. The ambient modules `ptc` and `ptc:tools` do not need resolution.
/// - `--singleThreaded`, `--locale en` and an empty environment: the diagnostics depend on
///   nothing but the files.
/// - `--pretty false`: one diagnostic per line, its chained messages indented below it.
///
/// At most [`MAX_OUTPUT`] bytes of diagnostics are read: past them, tsgo is stopped, and the
/// diagnostics are those of the complete lines read.
pub(crate) async fn run(
    executable: &Path,
    dir: &Path,
    timeout: Duration,
) -> Result<Output, CheckError> {
    let mut command = tokio::process::Command::new(executable);
    command
        .args([
            "--project",
            ".",
            "--pretty",
            "false",
            "--noResolve",
            "--singleThreaded",
            "--locale",
            "en",
        ])
        .current_dir(dir)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Dropped at the timeout: tsgo is killed with it.
    let ran = async {
        let mut child = command.spawn()?;
        let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take())
        else {
            return Err(io::Error::other("tsgo's output is not piped"));
        };
        let read_stdout = async {
            let mut output = Vec::new();
            (&mut stdout)
                .take(u64::try_from(MAX_OUTPUT).unwrap_or(u64::MAX) + 1)
                .read_to_end(&mut output)
                .await?;
            let truncated = output.len() > MAX_OUTPUT;
            if truncated {
                // Enough was read: the rest would only be dropped.
                child.start_kill()?;
                let complete = output.iter().rposition(|&byte| byte == b'\n');
                output.truncate(complete.map_or(0, |end| end + 1));
            }
            Ok::<_, io::Error>((output, truncated))
        };
        let ((stdout, truncated), stderr) =
            tokio::try_join!(read_stdout, read_kept(&mut stderr, MAX_ERRORS))?;
        let status = child.wait().await?;
        Ok((stdout, truncated, stderr, status))
    };
    let (stdout, truncated, stderr, status) = tokio::time::timeout(timeout, ran)
        .await
        .map_err(|_| CheckError::Timeout(timeout))?
        .map_err(CheckError::Start)?;
    let stdout = String::from_utf8_lossy(&stdout);
    let diagnostics = parse(&stdout);
    // 0: no diagnostics; 1 and 2: diagnostics; stopped once enough was read.
    let expected = match status.code() {
        _ if truncated => !diagnostics.is_empty(),
        Some(0) => true,
        Some(1 | 2) => !diagnostics.is_empty(),
        _ => false,
    };
    if !expected {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(CheckError::Failed {
            status: status.to_string(),
            output: format!("{}{}", stdout.trim(), stderr.trim()),
        });
    }
    Ok(Output { diagnostics })
}

/// Reads a stream to its end, keeping its first `limit` bytes: the writer is never blocked.
async fn read_kept(reader: &mut (impl AsyncRead + Unpin), limit: usize) -> io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(kept);
        }
        let room = limit.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..read.min(room)]);
    }
}

/// Reads the output of `--pretty false`:
///
/// ```text
/// script.ts(4,8): error TS2322: Type '(a: number) => void' is not assignable to type '(a: string) => void'.
///   Types of parameters 'a' and 'a' are incompatible.
/// error TS5023: Unknown compiler option 'x'.
/// ```
pub(crate) fn parse(output: &str) -> Vec<Reported> {
    let mut diagnostics: Vec<Reported> = Vec::new();
    for line in output.lines() {
        if line.starts_with(' ') {
            if let Some(last) = diagnostics.last_mut() {
                last.message.push('\n');
                last.message.push_str(line.trim_end());
            }
            continue;
        }
        if let Some(diagnostic) = parse_line(line) {
            diagnostics.push(diagnostic);
        }
    }
    diagnostics
}

fn parse_line(line: &str) -> Option<Reported> {
    let (location, rest) = match line.split_once("): ") {
        Some((head, rest)) if !head.starts_with("error") && !head.starts_with("warning") => {
            let (file, position) = head.rsplit_once('(')?;
            let (line, column) = position.split_once(',')?;
            (
                Some((file.to_owned(), line.parse().ok()?, column.parse().ok()?)),
                rest,
            )
        }
        _ => (None, line),
    };
    let (category, rest) = rest.split_once(" TS")?;
    let warning = match category {
        "error" => false,
        "warning" | "message" | "suggestion" => true,
        _ => return None,
    };
    let (code, message) = rest.split_once(": ")?;
    Some(Reported {
        location,
        warning,
        code: code.parse().ok()?,
        message: message.trim_end().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_diagnostics_with_their_chained_messages() {
        let output = "script.ts(4,8): error TS2322: Type '(a: number) => void' is not assignable to type '(a: string) => void'.\n  Types of parameters 'a' and 'a' are incompatible.\n    Type 'string' is not assignable to type 'number'.\nglobals.d.ts(100,13): error TS2403: Subsequent variable declarations must have the same type.  Variable 'URL' must be of type 'x'.\nerror TS5023: Unknown compiler option 'x'.\nscript.ts(1,2): warning TS6133: 'x' is declared but its value is never read.\n";
        let diagnostics = parse(output);
        assert_eq!(
            diagnostics,
            [
                Reported {
                    location: Some(("script.ts".into(), 4, 8)),
                    warning: false,
                    code: 2322,
                    message: "Type '(a: number) => void' is not assignable to type '(a: string) => void'.\n  Types of parameters 'a' and 'a' are incompatible.\n    Type 'string' is not assignable to type 'number'.".into(),
                },
                Reported {
                    location: Some(("globals.d.ts".into(), 100, 13)),
                    warning: false,
                    code: 2403,
                    message: "Subsequent variable declarations must have the same type.  Variable 'URL' must be of type 'x'.".into(),
                },
                Reported {
                    location: None,
                    warning: false,
                    code: 5023,
                    message: "Unknown compiler option 'x'.".into(),
                },
                Reported {
                    location: Some(("script.ts".into(), 1, 2)),
                    warning: true,
                    code: 6133,
                    message: "'x' is declared but its value is never read.".into(),
                },
            ]
        );
    }

    #[test]
    fn a_message_holding_a_location_is_read_whole() {
        let diagnostics = parse(
            "script.ts(2,3): error TS2304: Cannot find name 'f(1,2): x'.\nnot a diagnostic\n",
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].location, Some(("script.ts".into(), 2, 3)));
        assert_eq!(diagnostics[0].message, "Cannot find name 'f(1,2): x'.");
    }

    #[test]
    fn the_embedded_tarball_is_the_pinned_package() {
        assert_eq!(TSGO_VERSION, "7.0.2");
        assert!(INTEGRITY.starts_with("sha512-"));
        assert!(TARBALL.len() > 1_000_000);
        let mut archive = tar::Archive::new(GzDecoder::new(TARBALL));
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().display().to_string())
            .collect();
        assert!(names.contains(&"package/lib/tsc".to_owned()));
        assert!(names.contains(&"package/lib/lib.es2025.iterator.d.ts".to_owned()));
        assert!(names.contains(&"package/lib/lib.esnext.temporal.d.ts".to_owned()));
    }
}
