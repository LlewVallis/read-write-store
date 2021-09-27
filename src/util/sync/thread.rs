use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

#[cfg(not(loom))]
pub fn current_thread_hash() -> u64 {
    let mut hasher = DefaultHasher::new();
    std::thread::current().id().hash(&mut hasher);
    hasher.finish()
}

#[cfg(loom)]
pub fn current_thread_hash() -> u64 {
    let mut hasher = DefaultHasher::new();
    loom::thread::current().id().hash(&mut hasher);
    hasher.finish()
}
