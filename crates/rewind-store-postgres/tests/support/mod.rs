//! A Postgres server in Docker for the tests: a container with a unique name on a free local
//! port, removed when the test ends. Without Docker the tests are skipped, and say so, unless
//! `REWIND_REQUIRE_DOCKER` is set (CI): they fail then.

#![allow(dead_code, unreachable_pub)]

use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rewind_store_postgres::{Config, MasterKey, PostgresStore};

const IMAGE: &str = "postgres:17";
const USER: &str = "rewind";
const PASSWORD: &str = "rewind-secret";

/// A Postgres container, removed on drop.
pub struct Postgres {
    name: String,
    address: String,
    databases: AtomicUsize,
}

impl Postgres {
    /// Starts Postgres; `None` when Docker is absent. `test` names the container.
    pub fn start(test: &str) -> Option<Self> {
        let version = Command::new("docker")
            .args(["version", "--format", "{{.Server.Version}}"])
            .output();
        if !matches!(&version, Ok(output) if output.status.success()) {
            eprintln!(
                "SKIPPED: {test} needs Docker to run Postgres, and Docker is absent or its daemon \
                 is not running"
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
        let name = format!(
            "rewind-store-postgres-{test}-{}-{nanos}",
            std::process::id()
        );
        docker(&[
            "run",
            "--detach",
            "--rm",
            "--name",
            &name,
            "--publish",
            "127.0.0.1::5432",
            "--tmpfs",
            "/var/lib/postgresql/data",
            "--env",
            &format!("POSTGRES_USER={USER}"),
            "--env",
            &format!("POSTGRES_PASSWORD={PASSWORD}"),
            IMAGE,
            "-c",
            "max_connections=300",
        ]);
        // From here on, dropping `postgres` removes the container.
        let mut postgres = Postgres {
            name,
            address: String::new(),
            databases: AtomicUsize::new(0),
        };
        let port = docker(&["port", &postgres.name, "5432/tcp"]);
        postgres.address = String::from_utf8_lossy(&port.stdout)
            .lines()
            .next()
            .expect("a published port")
            .trim()
            .to_owned();
        postgres.wait_until_ready();
        Some(postgres)
    }

    /// The connection string of a database of this server.
    pub fn url(&self, database: &str) -> String {
        format!("postgres://{USER}:{PASSWORD}@{}/{database}", self.address)
    }

    /// Creates a new, empty database and returns its name.
    pub fn create_database(&self) -> String {
        let n = self.databases.fetch_add(1, Ordering::Relaxed);
        let database = format!("store_{n}");
        self.psql("postgres", &format!("CREATE DATABASE {database}"));
        database
    }

    /// Runs SQL with `psql` inside the container.
    pub fn psql(&self, database: &str, sql: &str) -> String {
        let output = docker(&[
            "exec",
            &self.name,
            "psql",
            "-U",
            USER,
            "-d",
            database,
            "-v",
            "ON_ERROR_STOP=1",
            "-tAc",
            sql,
        ]);
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// A store on a new, empty database.
    pub async fn store(&self) -> PostgresStore {
        let database = self.create_database();
        self.connect(&database).await
    }

    /// A store on an existing database.
    pub async fn connect(&self, database: &str) -> PostgresStore {
        self.connect_with(database, 4).await
    }

    /// A store on an existing database, with a pool of `connections`.
    pub async fn connect_with(&self, database: &str, connections: u32) -> PostgresStore {
        let mut config = Config::new(self.url(database));
        config.max_connections = connections;
        PostgresStore::connect(&config, MasterKey::from_bytes([1; 32]))
            .await
            .expect("connect a store")
    }

    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            // The image restarts the server once its initialization is done: wait for a query
            // through the published port, not only for the first `pg_isready`.
            let ready = Command::new("docker")
                .args([
                    "exec",
                    &self.name,
                    "psql",
                    "-h",
                    "127.0.0.1",
                    "-U",
                    USER,
                    "-d",
                    "postgres",
                    "-tAc",
                    "SELECT 1",
                ])
                .output();
            if matches!(&ready, Ok(output) if output.status.success()) {
                return;
            }
            assert!(Instant::now() < deadline, "Postgres did not start in time");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Postgres {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.name])
            .output();
    }
}

fn docker(args: &[&str]) -> Output {
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("run docker");
    assert!(
        output.status.success(),
        "docker {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
