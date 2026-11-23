//! `SlatePartitions` against MinIO in Docker: the conformance suite of `PartitionStore`, each
//! store under its own root in one bucket.
//!
//! The test starts a MinIO container with a unique name on a free local port, creates its bucket
//! with the MinIO client, and removes the container when it ends. Both images come from
//! Chainguard's builds, as `rewind-blob`'s test does. Without Docker the test is skipped, and
//! says so, unless `REWIND_REQUIRE_DOCKER` is set (CI): it fails then.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rewind_engine::conformance::partition;
use rewind_store_slatedb::{Bucket, Options, S3Bucket, SlatePartitions};

const MINIO_IMAGE: &str = "cgr.dev/chainguard/minio:latest";
const MC_IMAGE: &str = "cgr.dev/chainguard/minio-client:latest";
const USER: &str = "rewindslate";
const PASSWORD: &str = "rewindslate-secret";
const BUCKET: &str = "partitions";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slatedb_on_minio_passes_the_partition_suite() {
    let Some(minio) = Minio::start() else {
        return;
    };
    let stores = AtomicUsize::new(0);
    partition::run_all(|| {
        let n = stores.fetch_add(1, Ordering::Relaxed);
        SlatePartitions::new(&minio.bucket(&format!("store-{n}")), Options::default()).unwrap()
    })
    .await;
}

/// A MinIO container, removed on drop.
struct Minio {
    name: String,
    endpoint: String,
}

impl Minio {
    /// Starts MinIO and creates its bucket; `None` when Docker is absent.
    fn start() -> Option<Self> {
        let version = Command::new("docker")
            .args(["version", "--format", "{{.Server.Version}}"])
            .output();
        if !matches!(&version, Ok(output) if output.status.success()) {
            eprintln!(
                "SKIPPED: the S3 conformance test needs Docker to run MinIO, and Docker is \
                 absent or its daemon is not running"
            );
            assert!(
                std::env::var_os("REWIND_REQUIRE_DOCKER").is_none(),
                "REWIND_REQUIRE_DOCKER is set: Docker must be running"
            );
            return None;
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let name = format!("rewind-slatedb-minio-{}-{nanos}", std::process::id());
        docker(&[
            "run",
            "--detach",
            "--rm",
            "--name",
            &name,
            "--publish",
            "127.0.0.1::9000",
            "--tmpfs",
            "/data",
            "--env",
            &format!("MINIO_ROOT_USER={USER}"),
            "--env",
            &format!("MINIO_ROOT_PASSWORD={PASSWORD}"),
            MINIO_IMAGE,
            "server",
            "/data",
        ]);
        // From here on, dropping `minio` removes the container.
        let mut minio = Minio {
            name,
            endpoint: String::new(),
        };
        let port = docker(&["port", &minio.name, "9000/tcp"]);
        let address = String::from_utf8_lossy(&port.stdout)
            .lines()
            .next()
            .expect("a published port")
            .trim()
            .to_owned();
        minio.endpoint = format!("http://{address}");
        wait_until_live(&address);
        docker(&[
            "run",
            "--rm",
            "--network",
            &format!("container:{}", minio.name),
            "--env",
            &format!("MC_HOST_rewind=http://{USER}:{PASSWORD}@127.0.0.1:9000"),
            MC_IMAGE,
            "mb",
            &format!("rewind/{BUCKET}"),
        ]);
        Some(minio)
    }

    fn bucket(&self, root: &str) -> Bucket {
        Bucket::S3(S3Bucket {
            bucket: BUCKET.into(),
            region: Some("us-east-1".into()),
            endpoint: Some(self.endpoint.clone()),
            access_key_id: Some(USER.into()),
            secret_access_key: Some(PASSWORD.into()),
            session_token: None,
            allow_http: true,
            root: Some(root.into()),
        })
    }
}

impl Drop for Minio {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.name])
            .output();
    }
}

/// Runs a docker command that must succeed.
fn docker(args: &[&str]) -> Output {
    let output = Command::new("docker").args(args).output().expect("docker");
    assert!(
        output.status.success(),
        "docker {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Waits for MinIO's liveness probe to answer 200.
fn wait_until_live(address: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if live(address) {
            return;
        }
        assert!(Instant::now() < deadline, "MinIO did not start in 60 s");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn live(address: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect(address) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request =
        format!("GET /minio/health/live HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.starts_with("HTTP/1.1 200")
}
