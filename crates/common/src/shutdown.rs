//! Graceful-shutdown primitives: a cancellation tree + task tracking.
//! Bins create one [`Shutdown`], pass child tokens down, and `drain` on Ctrl-C.

use std::time::Duration;

pub use tokio_util::sync::CancellationToken;
pub use tokio_util::task::TaskTracker;

pub struct Shutdown {
    pub token: CancellationToken,
    pub tracker: TaskTracker,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    pub fn new() -> Self {
        Self { token: CancellationToken::new(), tracker: TaskTracker::new() }
    }

    pub fn child_token(&self) -> CancellationToken {
        self.token.child_token()
    }

    /// Cancel everything and wait for tracked tasks up to `deadline`.
    /// Returns false if the deadline expired with tasks still running.
    pub async fn drain(&self, deadline: Duration) -> bool {
        self.token.cancel();
        self.tracker.close();
        tokio::select! {
            _ = self.tracker.wait() => true,
            _ = tokio::time::sleep(deadline) => false,
        }
    }
}
