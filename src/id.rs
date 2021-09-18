//! Unique IDs for elements in [RwStores](crate::RwStore).
//!
//! See the [struct-level documentation](Id).

use std::fmt;
use std::fmt::{Debug, Formatter};
use std::ptr::NonNull;

use crate::rwstore_id::RwStoreId;

/// A handle used to read, modify or remove an element from an [RwStore](crate::RwStore)
/// once it has been inserted.
///
/// When debug assertions are enabled, IDs will consume more memory to track the store which that
/// created them.
///
/// # Example
///
/// ```
/// # use read_write_store::RwStore;
/// let store = RwStore::new();
/// let id_a = store.insert(42);
/// let id_b = store.insert(42);
/// ```
#[derive(Copy, Clone)]
pub struct Id {
    pub(crate) number: u32,
    pub(crate) slot_address: NonNull<()>,
    pub(crate) store_id: RwStoreId,
}

impl Id {
    pub(crate) fn new<T>(number: u32, slot: &T, store_id: RwStoreId) -> Self {
        Self {
            number,
            slot_address: NonNull::from(slot).cast(),
            store_id,
        }
    }
}

impl Debug for Id {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "Id({})", self.number)
    }
}

unsafe impl Send for Id {}

unsafe impl Sync for Id {}

#[cfg(test)]
mod test {
    use std::panic::{RefUnwindSafe, UnwindSafe};

    use crate::RwStore;

    #[test]
    fn debug_includes_id_number() {
        let store = RwStore::new();
        let id = store.insert(0);
        assert!(format!("{:?}", id).contains(&id.number.to_string()))
    }

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
