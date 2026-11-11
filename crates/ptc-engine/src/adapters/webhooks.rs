use std::sync::{Mutex, MutexGuard};

use crate::ports::{Subscription, WebhookError, WebhookEvent, WebhookSender};

/// `WebhookSender` in memory: records every delivery instead of sending it, for tests and mock
/// mode.
#[derive(Debug, Default)]
pub struct MemoryWebhooks {
    deliveries: Mutex<Vec<(Subscription, WebhookEvent)>>,
}

impl MemoryWebhooks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every delivery so far, in order.
    pub fn deliveries(&self) -> Vec<(Subscription, WebhookEvent)> {
        self.lock()
            .map(|deliveries| deliveries.clone())
            .unwrap_or_default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, Vec<(Subscription, WebhookEvent)>>, WebhookError> {
        self.deliveries.lock().map_err(|_| WebhookError::Delivery {
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
        self.lock()?.push((subscription.clone(), event.clone()));
        Ok(())
    }
}
