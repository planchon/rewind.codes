//! What `rewind server rename-data` does to a partition (`archi/06-storage-and-scale.md`,
//! "Rewriting the stored names"): every record read under the names it was written with and
//! written back in the new ones, by the family its key says it is (the keys keep their layout),
//! the gauges of each namespace counted again from the heads and summaries rewritten, and the
//! shape of the built-in `webhook` moved under the key of `signalUrl`.
//!
//! The partition is opened as any owner opens it, which fences the writer before it: the command
//! runs while no node serves. A record already in the new names is written back as it is, so
//! running it again writes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use rewind_core::{Digest, RunId};
use rewind_engine::ports::{Gauges, PartitionError};
use rewind_format::rename::{RecordKind, Renames};
use serde_json::Value;
use slatedb::bytes::Bytes;
use slatedb::config::{DurabilityLevel, ScanOptions};
use slatedb::{IsolationLevel, IterationOrder};

use crate::SlateLog;
use crate::codec;
use crate::keys::{self, Key};

/// What a partition's heads say of a run, in the names they were written with: what
/// `rename::workflows` and the archive's rewrite need.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredRun {
    pub tenant: String,
    pub run: RunId,
    pub namespace: String,
    pub code: Digest,
    pub action_set: Digest,
    /// The secrets it uses, by server.
    pub secrets: BTreeMap<String, String>,
    /// Its allowed labels.
    pub labels: BTreeMap<String, String>,
    /// The digest of the saved workflow version it ran, if its head names one.
    pub version: Option<Digest>,
    pub created_at: u64,
    /// The last attempt sealed into the archive.
    pub sealed: Option<u32>,
    /// A trigger waits in its inbox.
    pub stepping: bool,
}

/// What a rewrite of a partition changed, or would change, by kind of record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rewritten {
    pub records: BTreeMap<&'static str, u64>,
}

impl Rewritten {
    fn add(&mut self, kind: &'static str) {
        *self.records.entry(kind).or_default() += 1;
    }

    pub fn total(&self) -> u64 {
        self.records.values().sum()
    }
}

/// Every record of the partition, family by family, in key order.
async fn every_record(log: &SlateLog) -> Result<Vec<(Bytes, Bytes)>, PartitionError> {
    let options = ScanOptions {
        durability_filter: DurabilityLevel::Remote,
        order: IterationOrder::Ascending,
        ..crate::store::scan_options()
    };
    let mut records = Vec::new();
    for family in keys::KEY_FAMILIES {
        let range: (Bound<Vec<u8>>, Bound<Vec<u8>>) = (Bound::Unbounded, Bound::Unbounded);
        let mut iterator = log
            .db()
            .scan_prefix_with_options([family], range, &options)
            .await
            .map_err(|err| log.error(err))?;
        while let Some(entry) = iterator.next().await.map_err(|err| log.error(err))? {
            records.push((entry.key, entry.value));
        }
    }
    Ok(records)
}

fn corrupt(what: impl std::fmt::Display) -> PartitionError {
    PartitionError::Corrupt(what.to_string())
}

/// A string field, under its new name or its old one.
fn field<'a>(object: &'a Value, new: &str, old: &str) -> Option<&'a Value> {
    object.get(new).or_else(|| object.get(old))
}

fn digest_at(object: &Value, new: &str, old: &str) -> Result<Digest, PartitionError> {
    field(object, new, old)
        .and_then(Value::as_str)
        .and_then(|hex| Digest::from_hex(hex).ok())
        .ok_or_else(|| corrupt(format!("a run head without its {new}")))
}

fn strings(value: Option<&Value>) -> BTreeMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// The runs a partition holds, as their heads say, whatever names they were written with.
pub async fn stored_runs(log: &SlateLog) -> Result<Vec<StoredRun>, PartitionError> {
    let records = every_record(log).await?;
    let mut stepping = BTreeSet::new();
    let mut runs = Vec::new();
    for (key, value) in &records {
        match key.first() {
            Some(&keys::INBOX) => {
                // `i t r id`: the run follows the tenant.
                if let Some((_, rest)) = keys::read_str(&key[1..])
                    && let Some(run) = keys::run_from(rest)
                {
                    stepping.insert(run);
                }
            }
            Some(&keys::HEAD) => {
                let head: Value = serde_json::from_slice(value).map_err(corrupt)?;
                let run: RunId = head
                    .get("run")
                    .and_then(Value::as_str)
                    .and_then(|run| run.parse().ok())
                    .ok_or_else(|| corrupt("a run head without its run"))?;
                let metadata = head.get("metadata").cloned().unwrap_or_default();
                let workflow = metadata.get("workflow");
                // Before the rename release, a run of a saved version named its digest under
                // `version`; since, under `digest`.
                let version = workflow
                    .and_then(|workflow| {
                        workflow
                            .get("digest")
                            .or_else(|| workflow.get("version").filter(|v| v.is_string()))
                    })
                    .and_then(Value::as_str)
                    .and_then(|hex| Digest::from_hex(hex).ok());
                runs.push(StoredRun {
                    tenant: head
                        .get("tenant")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    run,
                    namespace: head
                        .get("namespace")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    code: digest_at(&head, "code", "script")?,
                    action_set: digest_at(&head, "action_set", "toolset")?,
                    secrets: strings(metadata.get("secrets")),
                    labels: strings(field(&metadata, "allowed_labels", "scope")),
                    version,
                    created_at: head.get("created_at").and_then(Value::as_u64).unwrap_or(0),
                    sealed: head
                        .get("sealed")
                        .and_then(Value::as_u64)
                        .and_then(|sealed| u32::try_from(sealed).ok()),
                    stepping: false,
                });
            }
            _ => {}
        }
    }
    for run in &mut runs {
        run.stepping = stepping.contains(&run.run);
    }
    Ok(runs)
}

/// The kind of the record under `key`, for [`rewind_format::rename::record`].
fn kind_of(key: &[u8]) -> Option<(RecordKind<'static>, &'static str)> {
    Some(match *key.first()? {
        keys::HEAD | keys::LIVE | keys::UNSEALED => (RecordKind::RunHead, "run heads"),
        keys::SUMMARY | keys::INDEX => (RecordKind::Summary, "run summaries"),
        keys::HISTORY => (RecordKind::HistoryEvent, "history events"),
        keys::INBOX => (RecordKind::InboxItem, "inbox items"),
        keys::TASK => (RecordKind::Task, "call tasks"),
        keys::TIMER | keys::DUE => (RecordKind::Timer, "timers"),
        keys::OUTBOX => (RecordKind::Outbox, "outbox entries"),
        _ => return None,
    })
}

/// Rewrites every record of the partition in the new names (`write: false` counts what would
/// change and writes nothing), then counts the gauges again. Returns what changed.
pub async fn rename_partition<R: Renames>(
    log: &SlateLog,
    renames: &R,
    write: bool,
) -> Result<Rewritten, PartitionError> {
    let records = every_record(log).await?;
    let mut rewritten = Rewritten::default();
    let mut puts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut deletes: Vec<Vec<u8>> = Vec::new();
    let mut heads = Vec::new();
    let mut summaries = BTreeMap::new();
    let mut old_gauges: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for (key, value) in &records {
        let renamed = match key.first().copied() {
            Some(keys::GAUGES) => {
                old_gauges.insert(key.to_vec(), value.to_vec());
                continue;
            }
            Some(family @ (keys::HOUR | keys::TENANT_HOUR)) => {
                let (tenant, namespace) = counters_scope(family, &key[1..])
                    .ok_or_else(|| corrupt("a counters key without its namespace"))?;
                let kind = RecordKind::Counters {
                    tenant: &tenant,
                    namespace: &namespace,
                };
                let renamed = rewind_format::rename::record(kind, value, renames)
                    .map_err(|err| corrupt(format!("counters: {err}")))?;
                (renamed, "counters")
            }
            Some(keys::TENANT_STATE) => {
                let (tenant, _) = keys::read_str(&key[1..])
                    .ok_or_else(|| corrupt("a tenant state key without its tenant"))?;
                let kind = RecordKind::TenantState { tenant: &tenant };
                let renamed = rewind_format::rename::record(kind, value, renames)
                    .map_err(|err| corrupt(format!("tenant state: {err}")))?;
                (renamed, "tenant states")
            }
            Some(keys::SHAPE) => {
                // `X t n action`: the shape of the built-in `webhook` is `signalUrl`'s.
                let moved = shape_key(&key[1..]);
                if let Some(moved) = moved {
                    rewritten.add("shapes");
                    deletes.push(key.to_vec());
                    puts.push((moved, value.to_vec()));
                }
                continue;
            }
            _ => match kind_of(key) {
                Some((kind, name)) => {
                    let renamed = rewind_format::rename::record(kind, value, renames)
                        .map_err(|err| corrupt(format!("{name}: {err}")))?;
                    (renamed, name)
                }
                None => continue,
            },
        };
        let (bytes, name) = renamed;
        match key.first().copied() {
            Some(keys::HEAD) => heads.push(bytes.clone()),
            Some(keys::SUMMARY) => {
                summaries.insert(key.to_vec(), bytes.clone());
            }
            _ => {}
        }
        if bytes.as_slice() != value.as_ref() {
            rewritten.add(name);
            puts.push((key.to_vec(), bytes));
        }
    }

    // The gauges, counted again from the heads and summaries as they are now.
    let mut gauges: BTreeMap<Vec<u8>, Gauges> = BTreeMap::new();
    let summary_of = |head: &rewind_engine::ports::RunHead| {
        summaries
            .get(keys::summary(&head.run).as_bytes())
            .map(|bytes| {
                codec::decode::<rewind_format::partition::Summary>(bytes)
                    .and_then(codec::summary_of)
            })
            .transpose()
    };
    for bytes in &heads {
        let head =
            codec::decode::<rewind_format::partition::RunHead>(bytes).and_then(codec::head_of)?;
        let summary = summary_of(&head)?;
        let key = keys::gauges(&head.run.tenant, &head.namespace).into_bytes();
        gauges
            .entry(key)
            .or_default()
            .count(&head, summary.as_ref());
    }
    for (key, counted) in &gauges {
        if *counted == Gauges::default() {
            continue;
        }
        let bytes = codec::encode(&codec::gauges_record(counted));
        if old_gauges.get(key) != Some(&bytes) {
            rewritten.add("gauges");
            puts.push((key.clone(), bytes));
        }
    }
    for key in old_gauges.keys() {
        if gauges
            .get(key)
            .is_none_or(|counted| *counted == Gauges::default())
        {
            rewritten.add("gauges");
            deletes.push(key.clone());
        }
    }

    if write && !(puts.is_empty() && deletes.is_empty()) {
        let transaction = log
            .db()
            .begin(IsolationLevel::Snapshot)
            .map_err(|err| log.error(err))?;
        for key in &deletes {
            transaction.delete(key).map_err(|err| log.error(err))?;
        }
        for (key, value) in &puts {
            transaction.put(key, value).map_err(|err| log.error(err))?;
        }
        if let Some(written) = transaction.commit().await.map_err(|err| log.error(err))? {
            log.db().flush().await.map_err(|err| log.error(err))?;
            written
                .await_durable()
                .await
                .map_err(|err| log.error(err))?;
        }
    }
    Ok(rewritten)
}

/// The tenant and the namespace of a counters key: `C t n hour` or `D t hour n`.
fn counters_scope(family: u8, rest: &[u8]) -> Option<(String, String)> {
    let (tenant, rest) = keys::read_str(rest)?;
    let namespace = if family == keys::HOUR {
        keys::read_str(rest)?.0
    } else {
        keys::read_str(rest.get(4..)?)?.0
    };
    Some((tenant, namespace))
}

/// The key of a shape of the built-in `webhook`, moved to `signalUrl`'s; `None` for another.
fn shape_key(rest: &[u8]) -> Option<Vec<u8>> {
    let (tenant, rest) = keys::read_str(rest)?;
    let (namespace, rest) = keys::read_str(rest)?;
    let (action, _) = keys::read_str(rest)?;
    (action == "webhook").then(|| {
        Key::new(keys::SHAPE)
            .str(&tenant)
            .str(&namespace)
            .str("signalUrl")
            .into_bytes()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shape_of_the_webhook_built_in_moves_to_signal_url() {
        let key = Key::new(keys::SHAPE)
            .str("acme")
            .str("alice")
            .str("webhook")
            .into_bytes();
        let moved = shape_key(&key[1..]).unwrap();
        assert_eq!(
            moved,
            Key::new(keys::SHAPE)
                .str("acme")
                .str("alice")
                .str("signalUrl")
                .into_bytes()
        );
        let other = Key::new(keys::SHAPE)
            .str("acme")
            .str("alice")
            .str("github.get")
            .into_bytes();
        assert_eq!(shape_key(&other[1..]), None);
    }

    #[test]
    fn a_counters_key_names_its_namespace() {
        let hour = Key::new(keys::HOUR)
            .str("acme")
            .str("alice")
            .u32(7)
            .into_bytes();
        assert_eq!(
            counters_scope(keys::HOUR, &hour[1..]),
            Some(("acme".into(), "alice".into()))
        );
        let tenant = Key::new(keys::TENANT_HOUR)
            .str("acme")
            .u32(7)
            .str("alice")
            .into_bytes();
        assert_eq!(
            counters_scope(keys::TENANT_HOUR, &tenant[1..]),
            Some(("acme".into(), "alice".into()))
        );
    }

    /// The records of a run in every status, as the build before the rename release wrote them
    /// (`rewind-format`'s fixtures `before-rename/partition/`): each reads back in the new names,
    /// an active run running or waiting from what its head holds, and the gauges count them
    /// again; run again, nothing changes.
    #[tokio::test]
    async fn every_status_reads_back_in_the_new_names() {
        use rewind_core::{PartitionId, RunStatus};
        use rewind_engine::ports::{
            NamespaceName, NodeId, PartitionLog, PartitionStore, RunRef, Scope, TenantId,
            WaitingFor,
        };
        use serde_json::json;

        let fixture = |name: &str| -> Value {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../rewind-format/tests/fixtures/before-rename/partition")
                .join(format!("{name}.v1.json"));
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        };
        let store =
            crate::SlatePartitions::new(&crate::Bucket::Memory, crate::Options::default()).unwrap();
        let log = store
            .open(PartitionId::new(0), &NodeId::new("n1").unwrap())
            .await
            .unwrap();
        let acme = TenantId::new("acme").unwrap();
        let signal = json!([{ "kind": "signal", "name": "approval", "since": 1 }]);
        let call = |kind: &str, task: &str| json!([{ "kind": kind, "seq": 2, "name": "github.merge", "since": 1, "task": task }]);
        let quarantine = json!({ "kind": "failures", "reason": "x", "since": 1 });
        // (old status, waits, quarantine, a trigger in its inbox, the new status)
        let runs = [
            ("pending", json!([]), None, true, RunStatus::Queued),
            ("active", signal.clone(), None, false, RunStatus::Waiting),
            ("active", signal.clone(), None, true, RunStatus::Running),
            (
                "active",
                call("call", "ready"),
                None,
                false,
                RunStatus::Running,
            ),
            (
                "active",
                call("call", "approval"),
                None,
                false,
                RunStatus::Waiting,
            ),
            (
                "active",
                call("host", "claimed"),
                None,
                false,
                RunStatus::Waiting,
            ),
            (
                "active",
                signal.clone(),
                Some(quarantine),
                true,
                RunStatus::Waiting,
            ),
            ("completed", json!([]), None, false, RunStatus::Completed),
            ("failed", json!([]), None, false, RunStatus::Failed),
            ("cancelled", json!([]), None, false, RunStatus::Cancelled),
            ("aborted", json!([]), None, false, RunStatus::Aborted),
        ];
        let transaction = log.db().begin(IsolationLevel::Snapshot).unwrap();
        let mut refs = Vec::new();
        for (n, (status, waits, quarantine, stepping, _)) in runs.iter().enumerate() {
            let id = RunId::from_u128(0x019b_7a3c_8f00_8003_8000_0000_0000_0100 + n as u128);
            let run = RunRef::new(acme.clone(), id);
            let mut head = fixture("run-head");
            head["run"] = json!(id.to_string());
            head["status"] = json!(status);
            head["waits"] = waits.clone();
            head["metadata"].as_object_mut().unwrap().remove("workflow");
            match quarantine {
                Some(quarantine) => head["quarantine"] = quarantine.clone(),
                None => {
                    head.as_object_mut().unwrap().remove("quarantine");
                }
            }
            let mut summary = fixture("summary");
            summary["run"] = json!(id.to_string());
            summary["status"] = json!(status);
            summary["waits"] = waits.clone();
            summary.as_object_mut().unwrap().remove("workflow");
            if *status == "active" || *status == "pending" {
                for field in ["ending", "ended_at"] {
                    summary.as_object_mut().unwrap().remove(field);
                }
                head.as_object_mut().unwrap().remove("ended_at");
            }
            match quarantine {
                Some(quarantine) => summary["quarantine"] = quarantine.clone(),
                None => {
                    summary.as_object_mut().unwrap().remove("quarantine");
                }
            }
            let head = head.to_string();
            transaction
                .put(keys::head(id).as_bytes(), head.as_bytes())
                .unwrap();
            if *status == "active" || *status == "pending" {
                transaction
                    .put(keys::live(id).as_bytes(), head.as_bytes())
                    .unwrap();
            }
            transaction
                .put(
                    keys::summary(&run).as_bytes(),
                    summary.to_string().as_bytes(),
                )
                .unwrap();
            if *stepping {
                let item = fixture("inbox-item");
                transaction
                    .put(keys::inbox(&run, 1).as_bytes(), item.to_string().as_bytes())
                    .unwrap();
            }
            refs.push(run);
        }
        let shape = fixture("shape");
        transaction
            .put(
                Key::new(keys::SHAPE)
                    .str("acme")
                    .str("alice")
                    .str("webhook")
                    .as_bytes(),
                shape.to_string().as_bytes(),
            )
            .unwrap();
        transaction
            .put(
                keys::gauges(&acme, &NamespaceName::new("alice").unwrap()).as_bytes(),
                fixture("gauges").to_string().as_bytes(),
            )
            .unwrap();
        let written = transaction.commit().await.unwrap().unwrap();
        log.db().flush().await.unwrap();
        written.await_durable().await.unwrap();

        let stored = stored_runs(&log).await.unwrap();
        assert_eq!(stored.len(), runs.len());
        let stepping: BTreeSet<RunId> = stored
            .iter()
            .filter(|run| run.stepping)
            .map(|run| run.run)
            .collect();
        struct Stepping(BTreeSet<RunId>);
        impl Renames for Stepping {
            fn digest(&self, _: &Digest) -> Option<Digest> {
                None
            }
            fn pinned(
                &self,
                _: &str,
                _: &str,
                _: &Digest,
            ) -> Option<rewind_format::rename::PinnedVersion> {
                None
            }
            fn run_workflow(&self, _: &RunId) -> Option<Value> {
                None
            }
            fn code_workflow(&self, _: &str, _: &str, _: &Digest) -> Option<String> {
                None
            }
            fn stepping(&self, run: &RunId) -> bool {
                self.0.contains(run)
            }
        }
        let renames = Stepping(stepping);
        let dry = rename_partition(&log, &renames, false).await.unwrap();
        let rewritten = rename_partition(&log, &renames, true).await.unwrap();
        assert_eq!(dry, rewritten, "a dry run counts what the run changes");
        assert_eq!(
            rewritten.records["run heads"],
            11 + 7,
            "the heads and the live heads"
        );
        assert_eq!(rewritten.records["shapes"], 1);
        for (run, (_, _, _, _, status)) in refs.iter().zip(runs.iter()) {
            let head = log.run(run).await.unwrap().unwrap();
            assert_eq!(head.status, *status, "{run}");
            let summary = log.summary(run).await.unwrap().unwrap();
            assert_eq!(summary.summary.status, *status, "{run}");
        }
        let gauges = log
            .gauges(&Scope::Namespace {
                tenant: acme.clone(),
                namespace: NamespaceName::new("alice").unwrap(),
            })
            .await
            .unwrap();
        assert_eq!(gauges.active_runs, 7);
        assert_eq!(
            (
                gauges.with_status(RunStatus::Queued),
                gauges.with_status(RunStatus::Running),
                gauges.with_status(RunStatus::Waiting)
            ),
            (1, 2, 4)
        );
        assert_eq!(gauges.waiting_for(WaitingFor::Quarantined), 1);
        assert_eq!(gauges.waiting_for(WaitingFor::Harness), 1);
        assert_eq!(gauges.waiting_for(WaitingFor::Approval), 1);
        let shapes = log
            .shapes(&acme, None, &["signalUrl".to_owned()])
            .await
            .unwrap();
        assert!(shapes.contains_key("signalUrl"), "{shapes:?}");
        let again = rename_partition(&log, &renames, true).await.unwrap();
        assert_eq!(again.total(), 0, "{again:?}");
    }
}
