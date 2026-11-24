//! The code executor: `rewind-runtime`'s `V8Executor`, whose every start and delivery pushes
//! back the embedded server's idle stop ([`Idle`]): an activation is work in progress.

use std::collections::BTreeSet;
use std::time::Duration;

use rewind_core::{ActionSet, Code, Epoch, Timestamp};
use rewind_engine::ports::{
    CodeExecutor, CodeInstance, CompiledCode, Emitted, ExecutorError, Handoff, InvalidCode,
    StartCode,
};
use rewind_runtime::{V8Executor, V8Instance};

use crate::idle::Idle;

/// The executor of this node.
#[derive(Debug)]
pub(crate) struct Executor {
    pub(crate) v8: V8Executor,
    pub(crate) idle: Idle,
}

impl CodeExecutor for Executor {
    type Instance = Instance;

    fn epochs(&self) -> &[Epoch] {
        self.v8.epochs()
    }

    fn compile(&self, code: &Code, action_set: &ActionSet) -> Result<CompiledCode, InvalidCode> {
        self.v8.compile(code, action_set)
    }

    fn action_imports(&self, code: &Code) -> Option<BTreeSet<String>> {
        self.v8.action_imports(code)
    }

    async fn start(&self, start: StartCode) -> Result<(Instance, Vec<Emitted>), ExecutorError> {
        self.idle.touch();
        let (v8, emitted) = self.v8.start(start).await?;
        Ok((
            Instance {
                v8,
                idle: self.idle.clone(),
            },
            emitted,
        ))
    }
}

/// A code instance of this node.
#[derive(Debug)]
pub(crate) struct Instance {
    v8: V8Instance,
    idle: Idle,
}

impl CodeInstance for Instance {
    async fn deliver(
        &mut self,
        delivery: Handoff,
        now: Timestamp,
    ) -> Result<Vec<Emitted>, ExecutorError> {
        self.idle.touch();
        self.v8.deliver(delivery, now).await
    }

    fn memory_usage(&self) -> u64 {
        self.v8.memory_usage()
    }

    fn cpu_time(&self) -> Duration {
        self.v8.cpu_time()
    }
}
