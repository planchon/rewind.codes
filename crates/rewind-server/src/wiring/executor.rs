//! The script executor: `rewind-runtime`'s `V8Executor`, whose every start and delivery pushes
//! back the embedded server's idle stop ([`Idle`]): an activation is work in progress.

use std::collections::BTreeSet;
use std::time::Duration;

use rewind_core::{ActionSet, Epoch, Script, Timestamp};
use rewind_engine::ports::{
    CompiledScript, Delivery, Emitted, ExecutorError, InvalidScript, ScriptExecutor,
    ScriptInstance, StartScript,
};
use rewind_runtime::{V8Executor, V8Instance};

use crate::idle::Idle;

/// The executor of this node.
#[derive(Debug)]
pub(crate) struct Executor {
    pub(crate) v8: V8Executor,
    pub(crate) idle: Idle,
}

impl ScriptExecutor for Executor {
    type Instance = Instance;

    fn epochs(&self) -> &[Epoch] {
        self.v8.epochs()
    }

    fn compile(
        &self,
        script: &Script,
        action_set: &ActionSet,
    ) -> Result<CompiledScript, InvalidScript> {
        self.v8.compile(script, action_set)
    }

    fn tool_imports(&self, script: &Script) -> Option<BTreeSet<String>> {
        self.v8.tool_imports(script)
    }

    async fn start(&self, start: StartScript) -> Result<(Instance, Vec<Emitted>), ExecutorError> {
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

/// A script instance of this node.
#[derive(Debug)]
pub(crate) struct Instance {
    v8: V8Instance,
    idle: Idle,
}

impl ScriptInstance for Instance {
    async fn deliver(
        &mut self,
        delivery: Delivery,
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
