//! Small, runtime-independent primitives shared by background services.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Cooperative cancellation and unique request identity. Clones refer to the
/// same request; cancellation does not publish any other memory or editor state.
#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Whether two handles identify the same request, including after cancellation.
    pub fn same_request(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
