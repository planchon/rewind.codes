use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::ports::{Subscription, WebhookError, WebhookEvent, WebhookSender};

/// `WebhookSender` in memory: records every delivery instead of sending it, for tests and mock
/// mode. Clones share their deliveries. A URL marked with [`refuse`](Self::refuse) refuses
/// every delivery, as a receiver that is down.
#[derive(Debug, Default, Clone)]
pub struct MemoryWebhooks {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    deliveries: Vec<(Subscription, WebhookEvent)>,
    refused: BTreeSet<String>,
}

impl MemoryWebhooks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every delivery so far, in order.
    pub fn deliveries(&self) -> Vec<(Subscription, WebhookEvent)> {
        self.lock()
            .map(|state| state.deliveries.clone())
            .unwrap_or_default()
    }

    /// Deliveries to `url` fail from now on.
    pub fn refuse(&self, url: impl Into<String>) {
        if let Ok(mut state) = self.lock() {
            state.refused.insert(url.into());
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, WebhookError> {
        self.state.lock().map_err(|_| WebhookError::Delivery {
            url: String::new(),
            message: "the memory webhook sender's lock is poisoned".into(),
        })
    }
}

impl WebhookSender for MemoryWebhooks {
    async fn send(
        &self,
        subscription: &Subscription,
        event: &WebhookEvent,
    ) -> Result<(), WebhookError> {
        let mut state = self.lock()?;
        if state.refused.contains(&subscription.url) {
            return Err(WebhookError::Delivery {
                url: subscription.url.clone(),
                message: "the receiver refused the delivery".into(),
            });
        }
        state.deliveries.push((subscription.clone(), event.clone()));
        Ok(())
    }
}
