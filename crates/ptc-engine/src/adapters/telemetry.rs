use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use crate::ports::{ExportError, LogRecord, Span, SpanExporter, SpanId, TraceId};

/// `SpanExporter` in memory: keeps what was exported, for tests. A span exported twice is kept
/// once, as the last export left it: spans have deterministic ids.
#[derive(Debug, Default)]
pub struct MemorySpanExporter {
    exported: Mutex<Exported>,
}

#[derive(Debug, Default)]
struct Exported {
    spans: BTreeMap<(TraceId, SpanId), Span>,
    logs: Vec<LogRecord>,
}

impl MemorySpanExporter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The spans exported, by trace and span id.
    pub fn spans(&self) -> Vec<Span> {
        self.lock()
            .map(|exported| exported.spans.values().cloned().collect())
            .unwrap_or_default()
    }

    /// The log records exported, in export order.
    pub fn logs(&self) -> Vec<LogRecord> {
        self.lock()
            .map(|exported| exported.logs.clone())
            .unwrap_or_default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, Exported>, ExportError> {
        self.exported
            .lock()
            .map_err(|_| ExportError::Export("the memory exporter's lock is poisoned".into()))
    }
}

impl SpanExporter for MemorySpanExporter {
    async fn export(&self, spans: Vec<Span>) -> Result<(), ExportError> {
        let mut exported = self.lock()?;
        for span in spans {
            exported.spans.insert((span.trace_id, span.span_id), span);
        }
        Ok(())
    }

    async fn export_logs(&self, logs: Vec<LogRecord>) -> Result<(), ExportError> {
        self.lock()?.logs.extend(logs);
        Ok(())
    }
}
