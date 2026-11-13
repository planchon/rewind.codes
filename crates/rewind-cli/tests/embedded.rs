//! The launcher of the embedded server: lock check, detached start, socket wait, version
//! handshake, replacement of a server of another version, a killed server, concurrent commands.
//!
//! The test binary is also the server it starts: run as `<test> server --embedded --dir <dir>
//! --idle-timeout 10m`,
//! it is a stub embedded server that takes `server.lock`, listens on `server.sock` and answers
//! `GET /v1/server` and `POST /v1/server/stop`, as `rewind server --embedded` does. Files in the
//! state directory steer it: `stub-version` (the version it reports), `stub-delay-ms` (before it
//! listens), `stub-fail` (it fails to start). It writes a line to `serving.log` once it holds
//! the lock.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use libtest_mimic::{Arguments, Failed, Trial};
use rewind_api::admin::{ServerInfo, ServerRole};
use rewind_cli::embedded::{LaunchError, Launcher, VERSION};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("server") {
        return stub_server(&args[2..]);
    }
    let tests = vec![
        trial(
            "starts_a_server_when_none_runs",
            starts_a_server_when_none_runs,
        ),
        trial("a_stale_socket_is_replaced", a_stale_socket_is_replaced),
        trial("a_killed_server_is_replaced", a_killed_server_is_replaced),
        trial(
            "a_server_of_another_version_is_replaced",
            a_server_of_another_version_is_replaced,
        ),
        trial(
            "concurrent_commands_share_one_server",
            concurrent_commands_share_one_server,
        ),
        trial("waits_for_a_slow_server", waits_for_a_slow_server),
        trial(
            "a_server_that_cannot_start_is_reported",
            a_server_that_cannot_start_is_reported,
        ),
    ];
    libtest_mimic::run(&Arguments::from_args(), tests).exit_code()
}

fn trial(name: &'static str, test: fn(&Path) -> Result<(), Failed>) -> Trial {
    Trial::test(name, move || {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let state = dir.path().join(".rewind");
        let result = test(&state);
        stop_any(&state);
        result
    })
}

fn launcher(state: &Path) -> Launcher {
    let program = std::env::current_exe().expect("the test binary");
    Launcher::with_program(state, program)
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(future)
}

fn check(condition: bool, message: impl Into<String>) -> Result<(), Failed> {
    if condition {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

/// The pids of the stub servers that held the lock, in order.
fn serving(state: &Path) -> Vec<u32> {
    std::fs::read_to_string(state.join("serving.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.parse().ok())
        .collect()
}

/// Whether a process runs. The servers the launcher started are children of this test
/// process, which never waits for them: a dead one stays a zombie until the test ends.
fn alive(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .is_ok_and(|stat| !stat.is_empty() && !stat.starts_with('Z'))
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) -> Result<(), Failed> {
    let deadline = Instant::now() + timeout;
    while !done() {
        if Instant::now() > deadline {
            return Err(format!("timed out waiting until {what}").into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Stops whatever stub still serves the directory, so that no test leaves a process behind.
fn stop_any(state: &Path) {
    let socket = state.join("server.sock");
    if socket.exists()
        && let Ok(client) = rewind_api::Client::embedded(&socket)
    {
        let _ = block_on(async {
            tokio::time::timeout(Duration::from_secs(2), client.stop_server()).await
        });
    }
    for pid in serving(state) {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn starts_a_server_when_none_runs(state: &Path) -> Result<(), Failed> {
    let launcher = launcher(state);
    let info = block_on(async {
        let client = launcher.connect().await?;
        client.server_info().await.map_err(LaunchError::from)
    })?;
    check(info.version == VERSION, format!("version {}", info.version))?;
    check(info.embedded, "the server is embedded")?;
    let pids = serving(state);
    check(pids.len() == 1, format!("one server started: {pids:?}"))?;

    // The socket is private, the lock is held, and the server is detached from our process
    // group: a Ctrl-C in the terminal does not reach it.
    let mode = std::fs::metadata(state.join("server.sock"))?
        .permissions()
        .mode()
        & 0o777;
    check(mode == 0o600, format!("socket mode {mode:o}"))?;
    check(!launcher.lock_is_free()?, "the lock is held")?;
    let group = |pid: u32| {
        std::process::Command::new("ps")
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    check(
        group(pids[0]) != group(std::process::id()),
        "the server has a process group of its own",
    )?;

    // A second command uses the same server.
    block_on(launcher.connect())?;
    check(serving(state).len() == 1, "the server is reused")?;
    Ok(())
}

fn a_stale_socket_is_replaced(state: &Path) -> Result<(), Failed> {
    std::fs::create_dir_all(state)?;
    std::fs::write(state.join("server.sock"), "left by a killed server")?;
    block_on(launcher(state).connect())?;
    check(serving(state).len() == 1, "a server started")?;
    Ok(())
}

fn a_killed_server_is_replaced(state: &Path) -> Result<(), Failed> {
    let launcher = launcher(state);
    block_on(launcher.connect())?;
    let first = serving(state)[0];
    std::process::Command::new("kill")
        .args(["-9", &first.to_string()])
        .status()?;
    wait_until("the killed server is gone", Duration::from_secs(10), || {
        !alive(first)
    })?;
    check(launcher.lock_is_free()?, "kill -9 released the lock")?;
    check(
        state.join("server.sock").exists(),
        "the killed server left its socket",
    )?;

    let info = block_on(async {
        let client = launcher.connect().await?;
        client.server_info().await.map_err(LaunchError::from)
    })?;
    check(info.version == VERSION, "the new server answers")?;
    let pids = serving(state);
    check(
        pids.len() == 2 && pids[1] != first,
        format!("a new server started: {pids:?}"),
    )?;
    Ok(())
}

fn a_server_of_another_version_is_replaced(state: &Path) -> Result<(), Failed> {
    std::fs::create_dir_all(state)?;
    std::fs::write(state.join("stub-version"), "0.0.1-old")?;
    block_on(launcher(state).version("0.0.1-old").connect())?;
    let old = serving(state)[0];

    std::fs::remove_file(state.join("stub-version"))?;
    let info = block_on(async {
        let client = launcher(state).connect().await?;
        client.server_info().await.map_err(LaunchError::from)
    })?;
    check(info.version == VERSION, format!("version {}", info.version))?;
    let pids = serving(state);
    check(
        pids.len() == 2,
        format!("the server was replaced: {pids:?}"),
    )?;
    wait_until("the old server is gone", Duration::from_secs(10), || {
        !alive(old)
    })?;
    Ok(())
}

fn concurrent_commands_share_one_server(state: &Path) -> Result<(), Failed> {
    let version = |launcher: Launcher| async move {
        let client = launcher.connect().await?;
        client
            .server_info()
            .await
            .map(|info| info.version)
            .map_err(LaunchError::from)
    };
    // Polled together on one thread: the launches interleave at every await.
    let versions = block_on(async {
        let (a, b, c, d) = tokio::join!(
            version(launcher(state)),
            version(launcher(state)),
            version(launcher(state)),
            version(launcher(state)),
        );
        [a, b, c, d]
    });
    for version in versions {
        check(version? == VERSION, "every command reached the server")?;
    }
    let pids = serving(state);
    check(
        pids.len() == 1,
        format!("one server holds the lock: {pids:?}"),
    )?;
    Ok(())
}

fn waits_for_a_slow_server(state: &Path) -> Result<(), Failed> {
    std::fs::create_dir_all(state)?;
    std::fs::write(state.join("stub-delay-ms"), "700")?;
    let started = Instant::now();
    block_on(launcher(state).connect())?;
    check(
        started.elapsed() >= Duration::from_millis(700),
        "the launcher waited for the socket",
    )?;
    Ok(())
}

fn a_server_that_cannot_start_is_reported(state: &Path) -> Result<(), Failed> {
    std::fs::create_dir_all(state)?;
    std::fs::write(state.join("stub-fail"), "")?;
    let error = block_on(launcher(state).connect()).expect_err("the server cannot start");
    let message = error.to_string();
    check(
        matches!(error, LaunchError::NotStarted { .. }),
        format!("not started: {message}"),
    )?;
    check(
        message.contains("cannot open journal.db: the disk is full"),
        format!("the server's log is quoted: {message}"),
    )?;
    Ok(())
}

// -------------------------------------------------------------------------------------------
// The stub embedded server

fn stub_server(args: &[String]) -> ExitCode {
    let dir = match args {
        [embedded, flag, dir, idle, timeout]
            if embedded == "--embedded"
                && flag == "--dir"
                && idle == "--idle-timeout"
                && timeout == "10m" =>
        {
            PathBuf::from(dir)
        }
        _ => {
            eprintln!("stub: usage: server --embedded --dir <dir> --idle-timeout 10m: {args:?}");
            return ExitCode::from(64);
        }
    };
    if dir.join("stub-fail").exists() {
        eprintln!("stub: cannot open journal.db: the disk is full");
        return ExitCode::from(70);
    }
    let lock = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("server.lock"))
    {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("stub: {error}");
            return ExitCode::from(70);
        }
    };
    if rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        eprintln!("stub: another server holds the lock");
        return ExitCode::from(1);
    }
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("serving.log"))
    {
        let _ = writeln!(log, "{}", std::process::id());
    }
    if let Ok(delay) = std::fs::read_to_string(dir.join("stub-delay-ms"))
        && let Ok(ms) = delay.trim().parse()
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
    let version = std::fs::read_to_string(dir.join("stub-version"))
        .map(|version| version.trim().to_owned())
        .unwrap_or_else(|_| VERSION.to_owned());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(serve(dir, version));
    drop(lock);
    ExitCode::SUCCESS
}

async fn serve(dir: PathBuf, version: String) {
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::{get, post};

    let socket = dir.join("server.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind the socket");
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).expect("socket mode");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let stop = std::sync::Mutex::new(Some(stop));
    let stop = std::sync::Arc::new(stop);
    let info = ServerInfo {
        version,
        epochs: vec![1],
        roles: vec![ServerRole::Api, ServerRole::Worker],
        embedded: true,
        virtual_time: false,
    };
    let app = Router::new()
        .route(
            "/v1/server",
            get(move || {
                let info = info.clone();
                async move { axum::Json(info) }
            }),
        )
        .route(
            "/v1/server/stop",
            post(move || {
                let stop = stop.clone();
                async move {
                    if let Some(stop) = stop.lock().ok().and_then(|mut stop| stop.take()) {
                        let _ = stop.send(());
                    }
                    StatusCode::NO_CONTENT
                }
            }),
        );
    // A stub never outlives its tests by long.
    let idle = tokio::time::sleep(Duration::from_secs(60));
    let shutdown = async move {
        tokio::select! {
            _ = stopped => {}
            () = idle => {}
        }
    };
    let _ = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await;
}
