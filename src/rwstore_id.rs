#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(debug_assertions)]
static NEXT_RWSTORE_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(debug_assertions)]
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct RwStoreId {
    value: u64,
}

#[cfg(debug_assertions)]
impl RwStoreId {
    pub fn generate() -> Self {
        let value = NEXT_RWSTORE_ID.fetch_add(1, Ordering::Relaxed);
        Self { value }
    }
}

#[cfg(not(debug_assertions))]
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct RwStoreId {
    _private: (),
}

#[cfg(not(debug_assertions))]
impl RwStoreId {
    pub fn generate() -> Self {
        Self { _private: () }
    }
}
