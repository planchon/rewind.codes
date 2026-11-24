//! `S3Blobs` and `S3Archive` against MinIO in Docker: the conformance suites of `BlobStore` and
//! `RunArchive`, alone and behind the local caches, the archive's keys, and corruption
//! detection.
//!
//! The test starts a MinIO container with a unique name on a free local port, creates its
//! buckets with the MinIO client, and removes the container when it ends. MinIO no longer
//! publishes its own images, so both come from Chainguard's builds. Without Docker the test is
//! skipped, and says so, unless `REWIND_REQUIRE_DOCKER` is set (CI): it fails then.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::{ObjectStoreExt as _, PutPayload};
use rewind_blob::{
    CachedArchive, CachedBlobs, DEFAULT_ARCHIVE_CACHE_CAPACITY, DEFAULT_CACHE_CAPACITY, S3Archive,
    S3Blobs, S3Config,
};
use rewind_core::{ActionSet, Code, Digest, Epoch, canonical_json};
use rewind_engine::conformance::{archive, blobs, tenant};
use rewind_engine::ports::{
    ArchiveError, Attempt, BlobError, BlobOwner, BlobStore, RunArchive, RunRef,
};
use serde_json::json;

const MINIO_IMAGE: &str = "cgr.dev/chainguard/minio:latest";
const MC_IMAGE: &str = "cgr.dev/chainguard/minio-client:latest";
const USER: &str = "rewindblob";
const PASSWORD: &str = "rewindblob-secret";
/// One bucket per store the suites create (12 checks of the blobs and 4 of the archive, alone
/// and behind the caches), one for the corruption test and one for the archive's keys.
const BUCKETS: usize = 34;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_blobs_pass_the_conformance_suite_on_minio() {
    let Some(minio) = Minio::start() else {
        return;
    };
    let buckets = AtomicUsize::new(0);
    let next_bucket = || {
        let n = buckets.fetch_add(1, Ordering::Relaxed);
        assert!(n < BUCKETS, "create more buckets");
        minio.config(&format!("blobs-{n}"))
    };

    blobs::run_all(|| S3Blobs::new(next_bucket()).expect("s3")).await;
    blobs::run_all(|| {
        CachedBlobs::new(
            S3Blobs::new(next_bucket()).expect("s3"),
            DEFAULT_CACHE_CAPACITY,
        )
    })
    .await;

    // A corrupt object is reported, not returned.
    let config = next_bucket();
    let store = S3Blobs::new(config.clone()).expect("s3");
    let raw = AmazonS3Builder::new()
        .with_bucket_name(&config.bucket)
        .with_region("us-east-1")
        .with_endpoint(config.endpoint.clone().expect("endpoint"))
        .with_access_key_id(USER)
        .with_secret_access_key(PASSWORD)
        .with_allow_http(true)
        .build()
        .expect("raw client");
    let bytes = canonical_json(&json!({ "text": "corrupt" })).into_bytes();
    let digest = Digest::of(&bytes);
    let key = S3Blobs::key(&BlobOwner::Tests(tenant("acme")), digest);
    raw.put(
        &Path::from(key.as_str()),
        PutPayload::from(b"{\"text\":\"tampered\"}".to_vec()),
    )
    .await
    .expect("tamper");
    let read = store.get(&BlobOwner::Tests(tenant("acme")), digest).await;
    assert!(
        matches!(read, Err(BlobError::Corrupt { digest: d, .. }) if d == digest),
        "{read:?}"
    );

    archive::run_all(|| {
        let blobs = S3Blobs::new(next_bucket()).expect("s3");
        (S3Archive::new(blobs.clone()), blobs)
    })
    .await;
    archive::run_all(|| {
        let blobs = S3Blobs::new(next_bucket()).expect("s3");
        (
            CachedArchive::new(
                S3Archive::new(blobs.clone()),
                DEFAULT_ARCHIVE_CACHE_CAPACITY,
            ),
            blobs,
        )
    })
    .await;

    // A sealed attempt is the run export at its key.
    let config = next_bucket();
    let store = S3Archive::new(S3Blobs::new(config.clone()).expect("s3"));
    let raw = AmazonS3Builder::new()
        .with_bucket_name(&config.bucket)
        .with_region("us-east-1")
        .with_endpoint(config.endpoint.clone().expect("endpoint"))
        .with_access_key_id(USER)
        .with_secret_access_key(PASSWORD)
        .with_allow_http(true)
        .build()
        .expect("raw client");
    let run = RunRef::new(
        tenant("Acme"),
        "01930000-0000-7000-8000-000000000001"
            .parse()
            .expect("run id"),
    );
    let sealed = rewind_engine::ports::SealedAttempt {
        run: run.clone(),
        attempt: Attempt::FIRST,
        code: Code::new("export default () => 1"),
        action_set: ActionSet::from_harness_actions(["a"]).expect("action set"),
        events: vec![rewind_core::HistoryEvent {
            index: 0,
            recorded_at: rewind_core::Timestamp(1),
            event: rewind_core::Event::Lifecycle(rewind_core::Lifecycle::RunStarted(
                rewind_core::RunStarted {
                    code: Code::new("export default () => 1").digest(),
                    action_set: ActionSet::from_harness_actions(["a"])
                        .expect("action set")
                        .digest(),
                    input: json!({}).into(),
                    seed: rewind_core::Seed::from_bytes([1; 32]),
                    epoch: Epoch(1),
                    expires_at: rewind_core::Timestamp(2),
                    mode: rewind_core::RunMode::Live,
                },
            )),
        }],
    };
    store.put(&sealed).await.expect("put");
    let key = S3Archive::key(&run, Attempt::FIRST);
    assert_eq!(key, "runs/Acme/01930000-0000-7000-8000-000000000001/1.json");
    let object = raw
        .get(&Path::from(key.as_str()))
        .await
        .expect("the object")
        .bytes()
        .await
        .expect("its bytes");
    let document =
        rewind_format::decode_run_export(std::str::from_utf8(&object).expect("utf-8")).expect("v1");
    assert_eq!(document, sealed.document());
    raw.put(
        &Path::from(key.as_str()),
        PutPayload::from(b"{\"v\":1}".to_vec()),
    )
    .await
    .expect("tamper");
    let read = store.get(&run, Attempt::FIRST).await;
    assert!(matches!(read, Err(ArchiveError::Corrupt(_))), "{read:?}");
}

/// A MinIO container, removed on drop.
struct Minio {
    name: String,
    endpoint: String,
}

impl Minio {
    /// Starts MinIO and creates its buckets; `None` when Docker is absent.
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
        // A counter keeps two containers of one process apart when the clock has only
        // microseconds.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "rewind-blob-minio-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
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

        let buckets: Vec<String> = (0..BUCKETS).map(|n| format!("rewind/blobs-{n}")).collect();
        let mut mb = vec![
            "run".to_owned(),
            "--rm".to_owned(),
            "--network".to_owned(),
            format!("container:{}", minio.name),
            "--env".to_owned(),
            format!("MC_HOST_rewind=http://{USER}:{PASSWORD}@127.0.0.1:9000"),
            MC_IMAGE.to_owned(),
            "mb".to_owned(),
        ];
        mb.extend(buckets);
        docker(&mb.iter().map(String::as_str).collect::<Vec<_>>());
        Some(minio)
    }

    fn config(&self, bucket: &str) -> S3Config {
        S3Config {
            region: Some("us-east-1".into()),
            endpoint: Some(self.endpoint.clone()),
            access_key_id: Some(USER.into()),
            secret_access_key: Some(PASSWORD.into()),
            allow_http: true,
            ..S3Config::new(bucket)
        }
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
