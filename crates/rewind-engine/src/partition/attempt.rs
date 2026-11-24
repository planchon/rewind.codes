//! A run's attempt read whole, as one state of its partition (`archi/06-storage-and-scale.md`,
//! "A partition", "The archive and the blobs").
//!
//! An attempt's records live in its partition until the attempt is sealed: the seal writes its
//! history to the archive, then a batch sets the head's `sealed` and deletes the attempt's
//! history and payload records. A reader that runs as a job of its own reads the head, then the
//! entries, in two reads: a seal committed between them would leave it a live head and no
//! entries, and it would answer an ended run with an empty history. So the head is read again
//! after the records: while it names the same live attempt, nothing deleted them (only a seal or
//! an erasure does), and the read holds; otherwise it starts over, and finds the attempt sealed,
//! the run's next attempt, or the run gone.

use rewind_core::HistoryEntry;

use crate::plane::{LogOf, Plane};
use crate::ports::{
    PartitionError, PartitionLog, PayloadRecord, RunArchive, RunHead, RunRef, SealedAttempt,
};

/// How many times a read starts over before it gives up: a seal happens once per attempt, so a
/// second read finds the attempt sealed.
const TRIES: usize = 3;

/// A run's current attempt, as its records were at one moment.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AttemptRead {
    /// The attempt is live, or ended and not sealed yet: its records in the partition.
    Live {
        head: RunHead,
        /// The entries from the index asked.
        entries: Vec<HistoryEntry>,
        /// The payload records of the payloads the entries hold by digest, when asked.
        payloads: Vec<PayloadRecord>,
    },
    /// The attempt is sealed: its history in the archive.
    Sealed {
        head: RunHead,
        sealed: Box<SealedAttempt>,
    },
}

impl AttemptRead {
    /// The entries from the index asked: every payload up to 64 KiB with its value once sealed,
    /// with `payloads` the values of the payload records otherwise.
    pub(crate) fn into_entries(self, from: u64) -> Vec<HistoryEntry> {
        match self {
            AttemptRead::Live {
                mut entries,
                payloads,
                ..
            } => {
                for entry in &mut entries {
                    if let Some(payload) = crate::payloads::payload_mut(&mut entry.event)
                        && let rewind_core::Payload::Digest(digest) = payload
                        && let Some(value) = payloads
                            .iter()
                            .find(|record| record.digest == *digest)
                            .and_then(|record| record.value.clone())
                    {
                        *payload = rewind_core::Payload::Value(value);
                    }
                }
                entries
            }
            AttemptRead::Sealed { sealed, .. } => sealed
                .entries
                .into_iter()
                .filter(|entry| entry.index >= from)
                .collect(),
        }
    }
}

/// Reads the run's current attempt from index `from`, with the payload records of its entries
/// when `payloads`; `None` when the run does not exist, or is being erased (its sealed attempt
/// is gone from the archive).
pub(crate) async fn read_attempt<P: Plane>(
    plane: &P,
    log: &LogOf<P>,
    run: &RunRef,
    from: u64,
    payloads: bool,
) -> Result<Option<AttemptRead>, PartitionError> {
    for _ in 0..TRIES {
        let Some(head) = log.run(run).await? else {
            return Ok(None);
        };
        if head.sealed == Some(head.attempt) {
            let sealed = plane
                .archive()
                .get(run, head.attempt)
                .await
                .map_err(|error| {
                    PartitionError::Unavailable(format!(
                        "the sealed attempt of run {} could not be read: {error}",
                        run.id
                    ))
                })?;
            return Ok(sealed.map(|sealed| AttemptRead::Sealed {
                head,
                sealed: Box::new(sealed),
            }));
        }
        let entries = log.history(run, head.attempt, from).await?;
        let mut records = Vec::new();
        if payloads {
            for digest in crate::executors::activation::digests_of(&entries) {
                records.extend(log.payload(run, digest).await?);
            }
        }
        // The records read are the attempt's while its head still names it live.
        let unchanged = log
            .run(run)
            .await?
            .is_some_and(|now| now.attempt == head.attempt && now.sealed != Some(head.attempt));
        if unchanged {
            return Ok(Some(AttemptRead::Live {
                head,
                entries,
                payloads: records,
            }));
        }
    }
    Err(PartitionError::Unavailable(format!(
        "the attempt of run {} changed at each read: try again",
        run.id
    )))
}
