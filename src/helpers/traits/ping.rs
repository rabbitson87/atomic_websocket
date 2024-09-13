use std::sync::Arc;

use tokio::sync::Mutex;

pub trait PingCheck {
    async fn is_wait_ping(&self) -> bool;
    async fn set_wait_ping(&self, value: bool);
}

impl PingCheck for Arc<Mutex<bool>> {
    async fn is_wait_ping(&self) -> bool {
        let clone = self.clone();
        let clone = clone.lock().await;
        let result = *clone == true;
        drop(clone);
        result
    }

    async fn set_wait_ping(&self, value: bool) {
        let clone = self.clone();
        let mut clone = clone.lock().await;
        *clone = value;
        drop(clone);
    }
}
