//! The names stored before the rename release (`rewind_format::rename`): every fixture as the
//! build before the release wrote it (`fixtures/before-rename/`, byte for byte) reads back in the
//! new names as the fixture this build writes, and a document already renamed comes back as it
//! is.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;

use rewind_core::{Digest, RunId};
use rewind_format::rename::{self, PinnedVersion, RecordKind, Renames};
use serde_json::{Value, json};

fn fixture(path: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(path);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{path:?}: {err}"));
    text.strip_suffix('\n').unwrap_or(&text).to_owned()
}

/// The digest of a document's bytes: the digest of a content-addressed document.
fn digest_of(path: &str) -> Digest {
    Digest::of(fixture(path).as_bytes())
}

/// The digests of the fixtures' documents, old to new, and what the partition fixtures need.
struct Fixtures {
    digests: BTreeMap<Digest, Digest>,
}

const TRIAGE: &str = "019b7a3c-8f00-8003-8000-000000000001";

impl Fixtures {
    fn new() -> Self {
        let mut digests = BTreeMap::new();
        for (old, new) in [
            ("before-rename/toolset.v1.json", "action-set.v1.json"),
            (
                "before-rename/toolset-apis.v1.json",
                "action-set-apis.v1.json",
            ),
            (
                "before-rename/toolset-workflows.v1.json",
                "action-set-workflows.v1.json",
            ),
            ("before-rename/workflow.v1.json", "workflow.v1.json"),
            (
                "before-rename/workflow-labels.v1.json",
                "workflow-labels.v1.json",
            ),
            (
                "before-rename/workflow-suite.v1.json",
                "workflow-suite.v1.json",
            ),
            (
                "before-rename/workflow-secrets.v1.json",
                "workflow-secrets.v1.json",
            ),
            ("before-rename/test.v1.json", "test.v1.json"),
        ] {
            digests.insert(digest_of(old), digest_of(new));
        }
        // The partition fixtures name an action set by the digest of its name.
        digests.insert(Digest::of(b"toolset"), Digest::of(b"action_set"));
        Self { digests }
    }
}

impl Renames for Fixtures {
    fn digest(&self, old: &Digest) -> Option<Digest> {
        self.digests.get(old).copied()
    }

    fn pinned(&self, tenant: &str, name: &str, version: &Digest) -> Option<PinnedVersion> {
        // The workflows the new fixture pins, as the catalog numbers them.
        let pinned: Value = serde_json::from_str(&fixture("action-set-workflows.v1.json")).unwrap();
        let workflow = pinned["workflows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|workflow| workflow["name"] == name)?;
        let _ = (tenant, version);
        Some(PinnedVersion {
            id: workflow["id"].as_str().unwrap().to_owned(),
            version: workflow["version"].as_u64().unwrap(),
            digest: Digest::from_hex(workflow["digest"].as_str().unwrap()).unwrap(),
        })
    }

    fn run_workflow(&self, run: &RunId) -> Option<Value> {
        (run.to_string() == TRIAGE).then(|| {
            json!({
                "id": "wf_7k2m9q4x8d1c",
                "name": "triage",
                "version": 12,
                "digest": "5ca4f3850ccc331aaf8a257d6086e526a3b42a63e18cb11d020847985b31d188",
                "interface": true
            })
        })
    }

    fn code_workflow(&self, _: &str, _: &str, code: &Digest) -> Option<String> {
        let ids = [
            (
                "21a0270b7f66a1e4c25933f13a1e5a1bbb4757578072930c8189131f9c6aaae1",
                "wf_7k2m9q4x8d1c",
            ),
            (
                "16367aacb67a4a017c8da8ab95682ccb390863780f7114dda0a0e0c55644c7c4",
                "wf_0d1g35t0000a",
            ),
        ];
        ids.iter()
            .find(|(hex, _)| code.to_hex() == *hex)
            .map(|(_, id)| (*id).to_owned())
    }

    fn stepping(&self, _: &RunId) -> bool {
        false
    }
}

/// The texts a record holds as they were written stay so: the new fixtures say "event" where
/// those written before the release said "entry".
fn as_written(new: &str) -> String {
    new.replace("replay mismatch at event 3", "replay mismatch at entry 3")
        .replace("file:///workflow.ts:12:5", "file:///script.ts:12:5")
        .replace(
            "\"name\":\"action github.close_issue\"",
            "\"name\":\"tool github.close_issue\"",
        )
}

#[test]
fn run_exports_read_in_the_new_names() {
    let renames = Fixtures::new();
    for name in [
        "aborted.v1.json",
        "completed-after-ttl.v1.json",
        "suspended-then-cancelled.v1.json",
    ] {
        let old = fixture(&format!("before-rename/{name}"));
        let renamed = rename::run_export(&old, "local", &renames).unwrap();
        let encoded = rewind_format::encode_run_export(&renamed);
        assert_eq!(encoded.trim_end(), as_written(&fixture(name)), "{name}");
        renamed
            .check()
            .unwrap_or_else(|err| panic!("{name}: {err}"));
        // Renamed again: nothing changes.
        let again = rename::run_export(&encoded, "local", &renames).unwrap();
        assert_eq!(rewind_format::encode_run_export(&again), encoded, "{name}");
    }
}

#[test]
fn documents_read_in_the_new_names_with_their_new_digests() {
    let renames = Fixtures::new();
    for (old, new) in [
        ("toolset.v1.json", "action-set.v1.json"),
        ("toolset-apis.v1.json", "action-set-apis.v1.json"),
        ("toolset-workflows.v1.json", "action-set-workflows.v1.json"),
    ] {
        let renamed =
            rename::action_set(&fixture(&format!("before-rename/{old}")), "acme", &renames)
                .unwrap();
        let encoded = rewind_format::encode_action_set(&renamed);
        assert_eq!(encoded, fixture(new), "{old}");
        assert_eq!(renamed.digest(), digest_of(new), "{old}");
        let again = rename::action_set(&encoded, "acme", &renames).unwrap();
        assert_eq!(again, renamed, "{old}: renamed again");
    }
    for name in [
        "workflow.v1.json",
        "workflow-labels.v1.json",
        "workflow-suite.v1.json",
        "workflow-secrets.v1.json",
    ] {
        let renamed =
            rename::workflow_version(&fixture(&format!("before-rename/{name}")), &renames).unwrap();
        assert_eq!(
            rewind_format::encode_workflow_version(&renamed),
            fixture(name),
            "{name}"
        );
    }
    let test = rename::test(&fixture("before-rename/test.v1.json"), &renames).unwrap();
    assert_eq!(rewind_format::encode_test(&test), fixture("test.v1.json"));
    let cassette = rename::cassette(&fixture("before-rename/cassette.v1.json")).unwrap();
    assert_eq!(
        rewind_format::encode_cassette(&cassette).trim_end(),
        fixture("cassette.v1.json")
    );
}

#[test]
fn partition_records_read_in_the_new_names() {
    let renames = Fixtures::new();
    let counters = RecordKind::Counters {
        tenant: "acme",
        namespace: "alice",
    };
    for (old, new, kind) in [
        ("run-head", "run-head", RecordKind::RunHead),
        ("entry", "history-event", RecordKind::HistoryEvent),
        ("inbox-item", "inbox-item", RecordKind::InboxItem),
        ("task", "task", RecordKind::Task),
        ("timer", "timer", RecordKind::Timer),
        ("summary", "summary", RecordKind::Summary),
        ("counters", "counters", counters),
        ("outbox-webhooks", "outbox-webhooks", RecordKind::Outbox),
        ("outbox-otel", "outbox-otel", RecordKind::Outbox),
        (
            "outbox-quota-alert",
            "outbox-quota-alert",
            RecordKind::Outbox,
        ),
        (
            "organization",
            "tenant-state",
            RecordKind::TenantState { tenant: "acme" },
        ),
        ("payload", "payload", RecordKind::Unchanged),
        ("watcher", "watcher", RecordKind::Unchanged),
        ("webhook-token", "webhook-token", RecordKind::Unchanged),
        ("start-key", "start-key", RecordKind::Unchanged),
        ("signal-key", "signal-key", RecordKind::Unchanged),
        ("shape", "shape", RecordKind::Unchanged),
    ] {
        let before = fixture(&format!("before-rename/partition/{old}.v1.json"));
        let renamed = rename::record(kind, before.as_bytes(), &renames)
            .unwrap_or_else(|err| panic!("{old}: {err}"));
        let renamed = String::from_utf8(renamed).unwrap();
        assert_eq!(
            renamed,
            as_written(&fixture(&format!("partition/{new}.v1.json"))),
            "{old}"
        );
        let again = rename::record(kind, renamed.as_bytes(), &renames).unwrap();
        assert_eq!(
            String::from_utf8(again).unwrap(),
            renamed,
            "{old}: renamed again"
        );
    }
}

#[test]
fn an_active_run_is_running_or_waiting_from_what_its_head_holds() {
    struct Stepping(bool);
    impl Renames for Stepping {
        fn digest(&self, _: &Digest) -> Option<Digest> {
            None
        }
        fn pinned(&self, _: &str, _: &str, _: &Digest) -> Option<PinnedVersion> {
            None
        }
        fn run_workflow(&self, _: &RunId) -> Option<Value> {
            None
        }
        fn code_workflow(&self, _: &str, _: &str, _: &Digest) -> Option<String> {
            None
        }
        fn stepping(&self, _: &RunId) -> bool {
            self.0
        }
    }
    let head = |status: &str, quarantine: Value, waits: Value| {
        let mut head: Value =
            serde_json::from_str(&fixture("before-rename/partition/run-head.v1.json")).unwrap();
        head["status"] = json!(status);
        head["quarantine"] = quarantine;
        head["waits"] = waits;
        if head["quarantine"].is_null() {
            head.as_object_mut().unwrap().remove("quarantine");
        }
        head.to_string()
    };
    let status = |head: &str, stepping: bool| -> String {
        let renamed =
            rename::record(RecordKind::RunHead, head.as_bytes(), &Stepping(stepping)).unwrap();
        let renamed: Value = serde_json::from_slice(&renamed).unwrap();
        renamed["status"].as_str().unwrap().to_owned()
    };
    let wait = |kind: &str, task: Option<&str>| {
        let mut wait = json!({ "kind": kind, "seq": 2, "name": "x", "since": 1 });
        if let Some(task) = task {
            wait["task"] = json!(task);
        }
        wait
    };
    let signal = json!([{ "kind": "signal", "name": "approval", "since": 1 }]);
    assert_eq!(
        status(&head("pending", Value::Null, json!([])), false),
        "queued"
    );
    assert_eq!(
        status(&head("active", Value::Null, signal.clone()), false),
        "waiting"
    );
    assert_eq!(
        status(&head("active", Value::Null, signal.clone()), true),
        "running"
    );
    assert_eq!(
        status(
            &head(
                "active",
                Value::Null,
                json!([wait("host", Some("claimed"))])
            ),
            false
        ),
        "waiting",
        "a harness action's call waits for the harness"
    );
    assert_eq!(
        status(
            &head("active", Value::Null, json!([wait("call", Some("ready"))])),
            false
        ),
        "running",
        "a call Rewind makes itself"
    );
    assert_eq!(
        status(
            &head("active", Value::Null, json!([wait("call", Some("parked"))])),
            false
        ),
        "waiting"
    );
    let quarantine = json!({ "kind": "failures", "reason": "x", "since": 1 });
    assert_eq!(
        status(
            &head("active", quarantine, json!([wait("call", Some("ready"))])),
            true
        ),
        "waiting",
        "a quarantined run waits for its release"
    );
    assert_eq!(
        status(&head("completed", Value::Null, json!([])), false),
        "completed"
    );
}
