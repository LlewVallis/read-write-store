use crate::header::RESERVED_ID;
use crate::util::sync::atomic::{AtomicU32, Ordering};

pub struct IdGenerator {
    // Should only be increased, should not be read from
    next_id: AtomicU32,
}

impl IdGenerator {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU32::new(RESERVED_ID + 1),
        }
    }

    pub fn next(&self) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        debug_assert!(id != RESERVED_ID, "element IDs have been exhausted");
        id
    }
}
