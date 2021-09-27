//! An opaque handle used to read, modify or remove an element from an [RwStore](crate::RwStore)
//! once it has been inserted.
//!
//! When debug assertions are enabled, IDs will consume more memory to track the store that created
//! them.
//!
//! # Example
//!
//! ```
//! # use read_write_store::RwStore;
//! let store = RwStore::new();
//! let id_a = store.insert(42);
//! let id_b = store.insert(42);
//! ```

use std::fmt;
use std::fmt::{Debug, Formatter};
use std::ptr::NonNull;

use crate::rwstore_id::RwStoreId;

/// An opaque handle used to read, modify or remove an element from an [RwStore](crate::RwStore)
/// once it has been inserted. See the [module-level documentation](crate::id) for more.
#[derive(Copy, Clone)]
pub struct Id {
    ordinal: u32,
    bucket_id: u32,
    slot_address: NonNull<()>,
    store_id: RwStoreId,
}

impl Id {
    pub(crate) fn new<T>(ordinal: u32, bucket_id: u32, slot: &T, store_id: RwStoreId) -> Self {
        Self {
            ordinal,
            bucket_id,
            slot_address: NonNull::from(slot).cast(),
            store_id,
        }
    }

    pub(crate) fn ordinal(&self) -> u32 {
        self.ordinal
    }

    pub(crate) fn bucket_id(&self) -> u32 {
        self.bucket_id
    }

    pub(crate) fn slot<T>(&self) -> NonNull<T> {
        self.slot_address.cast()
    }

    pub(crate) fn store_id(&self) -> RwStoreId {
        self.store_id
    }
}

impl Debug for Id {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_tuple("Id")
            .field(&self.ordinal)
            .field(&self.bucket_id)
            .field(&self.slot_address)
            .finish()
    }
}

unsafe impl Send for Id {}

unsafe impl Sync for Id {}

#[cfg(test)]
mod test {
    use std::panic::{RefUnwindSafe, UnwindSafe};

    use crate::RwStore;

    #[test]
    fn implements_sync() {
        let store = RwStore::new();
        let id = store.insert(0);
        &id as &dyn Sync;
    }

    #[test]
    fn implements_send() {
        let store = RwStore::new();
        let id = store.insert(0);
        &id as &dyn Send;
    }

    #[test]
    fn implements_unwind_safe() {
        let store = RwStore::new();
        let id = store.insert(0);
        &id as &dyn UnwindSafe;
    }

    #[test]
    fn implements_ref_unwind_safe() {
        let store = RwStore::new();
        let id = store.insert(0);
        &id as &dyn RefUnwindSafe;
    }
}
