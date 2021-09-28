//! A concurrent, unordered collection where each element has an internally generated ID and a
//! read-write lock.
//!
//! A store has O(1) time complexity for insertion, removal and lookups, although memory
//! allocations triggered by insertion may cause a performance spike.
//!
//! # Example
//!
//! ```
//! # use read_write_store::RwStore;
//! # use read_write_store::timeout::Timeout::DontBlock;
//! // Note that we only need an immutable reference to the store
//! let store = RwStore::new();
//!
//! // Inserting an element yields an ID
//! let id = store.insert(42);
//!
//! {
//! // You can read the element's value using that ID
//! let read_lock = store.read(id).unwrap();
//! assert_eq!(*read_lock, 42);
//!
//! // Concurrent reads are possible
//! assert!(store.read_with_timeout(id, DontBlock).is_ok());
//! // But reading and writing at the same time won't work
//! assert!(store.write_with_timeout(id, DontBlock).is_err());
//! }
//!
//! {
//! // You can also acquire a write lock using an ID
//! let mut write_lock = store.write(id).unwrap();
//! *write_lock = 24;
//! assert_eq!(*write_lock, 24);
//!
//! // Predictably, you cannot have multiple writers
//! assert!(store.write_with_timeout(id, DontBlock).is_err());
//! }
//!
//! // Elements can of course be removed using their ID
//! assert_eq!(store.remove(id), Some(24));
//!
//! // Now if we try to read using the ID, it will fail gracefully
//! assert!(store.read(id).is_none());
//! ```
//!
//! # Allocation behavior
//!
//! * An empty store does not require any allocation.
//! * Once an element is inserted, a fixed size block of element slots will be allocated.
//! * As more space is needed, more blocks will be allocated which tend to increase in size. All
//!   allocated blocks are used alongside all previously allocated blocks; elements are never moved.
//! * All removed elements whose memory is ready for reuse are tracked, so removing an element may
//!   allocate.
//!
//! # Caveats
//!
//! * When a block of elements is allocated internally, it won't be deallocated until the store is
//!   dropped. This allows the element ID's to contain a pointer to the element that is valid for
//!   the lifetime of the store.
//! * Every element has 8 bytes of overhead.
//! * The store itself has a non-trivial memory overhead.
//! * For performance reasons, no facilities are provided for querying the number of elements in the
//!   store.
//!
//! # Safety
//!
//! Each element inserted into a store is given an *internal* unique ID. This begs the question,
//! what happens when an ID from one store is used with another? When debug assertions are enabled,
//! a best effort attempt is made to panic when using an ID from a different store, but when they
//! are disabled (i.e. in release mode), or if this best effort fails, this
//! *may cause undefined behavior*.

#![feature(option_result_unwrap_unchecked)]
#![feature(ptr_metadata)]
#![feature(maybe_uninit_array_assume_init)]
#![feature(maybe_uninit_uninit_array)]
#![feature(option_zip)]
#![warn(missing_docs)]

use std::panic::{RefUnwindSafe, UnwindSafe};
use std::ptr::NonNull;

use bucket::Bucket;

use crate::block::Block;
pub use crate::id::*;
use crate::iter::{IntoIter, IterMut};
use crate::lock::{ReadLock, WriteLock};
use crate::rwstore_id::RwStoreId;
pub use crate::timeout::*;
use crate::timeout::{BlockResult, Timeout};
use crate::util::sync::concurrent_queue::ConcurrentQueue;
use crossbeam_utils::CachePadded;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use util::sync::thread;

mod block;
mod bucket;
mod header;
pub mod id;
pub mod iter;
pub mod lock;
#[cfg(all(test, loom))]
mod loom;
mod rwstore_id;
pub mod timeout;
mod util;

pub use header::MAX_CONCURRENT_READS;

const BUCKET_COUNT: usize = 16;

/// A concurrent, unordered collection where each element has an internally generated ID and a
/// read-write lock. See the [module-level documentation](crate) for more.
pub struct RwStore<Element> {
    buckets: [CachePadded<Bucket<Element>>; BUCKET_COUNT],
    // Contains slots ready for insertion
    erasures: ConcurrentQueue<Erasure>,
    // A globally unique ID for the store to ensure IDs from other stores are not used with this
    // one. When debug assertions are disabled this is elided
    store_id: RwStoreId,
}

impl<Element> RwStore<Element> {
    /// Creates a new, empty store.
    pub fn new() -> Self {
        let store_id = RwStoreId::generate();

        let buckets = unsafe {
            let mut buckets = MaybeUninit::uninit_array();

            for bucket in &mut buckets {
                *bucket = MaybeUninit::new(CachePadded::new(Bucket::new(store_id)));
            }

            MaybeUninit::array_assume_init(buckets)
        };

        Self {
            store_id,
            buckets,
            erasures: ConcurrentQueue::new(),
        }
    }

    /// Inserts an element into the store, returning it's generated unique ID. The returned ID can
    /// be used to subsequently read, modify or remove the element.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::RwStore;
    /// let store = RwStore::new();
    /// let id = store.insert(42);
    /// ```
    pub fn insert(&self, element: Element) -> Id {
        let slot_address = if let Some(erasure) = self.erasures.pop() {
            erasure.slot_address
        } else {
            let bucket_id = self.arbitrary_bucket_id();
            let bucket = &self.buckets[bucket_id as usize];
            let slot_address = bucket.next_insert_location();
            slot_address
        };

        unsafe { Block::insert(slot_address, element, self.store_id) }
    }

    /// Removes an element from the store using its ID if it has not already been removed. Returns
    /// the element if it was present.
    ///
    /// If a read or write lock is held on the element, this will block until it is released.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::RwStore;
    /// let store = RwStore::new();
    /// let id = store.insert(42);
    /// assert_eq!(store.remove(id), Some(42));
    /// ```
    ///
    /// # Safety
    ///
    /// The given ID must have been created by this store, see the
    /// [struct-level documentation](RwStore).
    pub fn remove(&self, id: Id) -> Option<Element> {
        self.remove_with_timeout(id, Timeout::BlockIndefinitely)
            .unwrap()
    }

    /// Removes an element from the store using its ID if it has not already been removed. Returns
    /// the element if it was present.
    ///
    /// If a read or write lock is held on the element, this will block until it is released or the
    /// given timeout expires.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::RwStore;
    /// # use read_write_store::timeout::Timeout::DontBlock;
    /// let store = RwStore::new();
    /// let id = store.insert(42);
    /// let read_lock = store.read(id);
    /// assert!(store.remove_with_timeout(id, DontBlock).is_err());
    /// ```
    ///
    /// # Safety
    ///
    /// The given ID must have been created by this store, see the
    /// [struct-level documentation](RwStore).
    pub fn remove_with_timeout(&self, id: Id, timeout: Timeout) -> BlockResult<Option<Element>> {
        self.assert_native_id(id);

        unsafe {
            if let Some(result) = Block::<Element>::remove(id, timeout)? {
                if result.may_reuse {
                    self.push_erasure(id);
                }

                Ok(Some(result.element))
            } else {
                Ok(None)
            }
        }
    }

    /// Removes an element from the store directly using a write lock held over the element. This is
    /// likely more efficient than unlocking and removing the element, and is guaranteed to succeed
    /// atomically, without blocking.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::RwStore;
    /// let store = RwStore::new();
    /// let id = store.insert(42);
    /// let write_lock = store.write(id).unwrap();
    /// assert_eq!(store.remove_locked(write_lock), 42);
    /// ```
    ///
    /// # Safety
    ///
    /// The given lock must have been acquired from one of the locking methods on this store. Using
    /// a lock from another store will panic when debug assertions are enabled, but *may cause
    /// undefined behavior* when they are disabled (i.e. in release mode).
    pub fn remove_locked(&self, lock: WriteLock<Element>) -> Element {
        let id = lock.forget();

        self.assert_native_id(id);

        let result = unsafe { Block::<Element>::remove_locked(id) };

        if result.may_reuse {
            self.push_erasure(id);
        }

        result.element
    }

    fn push_erasure(&self, id: Id) {
        self.erasures.push(Erasure {
            slot_address: id.slot(),
        });
    }

    /// Acquires a read lock on an element given its ID, if it is still present in the store.
    ///
    /// If a write lock is held on the element, this will block until it is released.
    ///
    /// This may be called reentrantly, but acquiring more than 2<sup>31</sup> - 2 concurrent read
    /// locks on the same element will panic.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::RwStore;
    /// let store = RwStore::new();
    /// let id = store.insert(42);
    /// let read_lock = store.read(id).unwrap();
    /// assert_eq!(*read_lock, 42);
    /// ```
    ///
    /// # Safety
    ///
    /// The given ID must have been created by this store, see the
    /// [struct-level documentation](RwStore).
    pub fn read(&self, id: Id) -> Option<ReadLock<Element>> {
        self.read_with_timeout(id, Timeout::BlockIndefinitely)
            .unwrap()
    }

    /// Acquires a read lock on an element given its ID, if it is still present in the store.
    ///
    /// If a write lock is held on the element, this will block until it is released or the given
    /// timeout expires.
    ///
    /// This may be called reentrantly, but acquiring more than 2<sup>31</sup> - 2 concurrent read
    /// locks on the same element will panic.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::RwStore;
    /// # use read_write_store::timeout::Timeout::DontBlock;
    /// let store = RwStore::new();
    /// let id = store.insert(42);
    /// let write_lock = store.write(id).unwrap();
    /// assert!(store.read_with_timeout(id, DontBlock).is_err());
    /// ```
    ///
    /// # Safety
    ///
    /// The given ID must have been created by this store, see the
    /// [struct-level documentation](RwStore).
    pub fn read_with_timeout(
        &self,
        id: Id,
        timeout: Timeout,
    ) -> BlockResult<Option<ReadLock<Element>>> {
        self.assert_native_id(id);

        unsafe {
            if Block::<Element>::lock_read(id, timeout)? {
                let lock = ReadLock::new(id);
                Ok(Some(lock))
            } else {
                Ok(None)
            }
        }
    }

    /// Acquires a write lock on an element given its ID, if it is still present in the store.
    ///
    /// If a read or write lock is held on the element, this will block until it is released.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::RwStore;
    /// let store = RwStore::new();
    /// let id = store.insert(42);
    /// let mut write_lock = store.write(id).unwrap();
    /// *write_lock = 24;
    /// ```
    ///
    /// # Safety
    ///
    /// The given ID must have been created by this store, see the
    /// [struct-level documentation](RwStore).
    pub fn write(&self, id: Id) -> Option<WriteLock<Element>> {
        self.write_with_timeout(id, Timeout::BlockIndefinitely)
            .unwrap()
    }

    /// Acquires a write lock on an element given its ID, if it is still present in the store.
    ///
    /// If a read or write lock is held on the element, this will block until it is released or the
    /// given timeout expires.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::RwStore;
    /// # use read_write_store::timeout::Timeout::DontBlock;
    /// let store = RwStore::new();
    /// let id = store.insert(42);
    /// let read_lock = store.read(id).unwrap();
    /// assert!(store.write_with_timeout(id, DontBlock).is_err());
    /// ```
    ///
    /// # Safety
    ///
    /// The given ID must have been created by this store, see the
    /// [struct-level documentation](RwStore).
    pub fn write_with_timeout(
        &self,
        id: Id,
        timeout: Timeout,
    ) -> BlockResult<Option<WriteLock<Element>>> {
        self.assert_native_id(id);

        unsafe {
            if Block::<Element>::lock_write(id, timeout)? {
                let lock = WriteLock::new(id);
                Ok(Some(lock))
            } else {
                Ok(None)
            }
        }
    }

    /// Directly obtains a mutable reference to an element given its ID, if it is still present in
    /// the store.
    ///
    /// Because a mutable reference is held over the store, this can avoid the overhead of locking.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::RwStore;
    /// let mut store = RwStore::new();
    /// let id = store.insert(42);
    /// assert_eq!(store.get_mut(id), Some(&mut 42));
    /// ```
    ///
    /// # Safety
    ///
    /// The given ID must have been created by this store, see the
    /// [struct-level documentation](RwStore).
    pub fn get_mut(&mut self, id: Id) -> Option<&mut Element> {
        self.assert_native_id(id);

        unsafe { Block::<Element>::get_exclusive(id).map(|mut ptr| ptr.as_mut()) }
    }

    /// Directly obtains a mutable reference to an element given its ID, assuming it is still
    /// present in the store.
    ///
    /// Because a mutable reference is held over the store, this can avoid the overhead of locking.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::RwStore;
    /// let mut store = RwStore::new();
    /// let id = store.insert(42);
    ///
    /// unsafe {
    ///     assert_eq!(store.get_mut_unchecked(id), &mut 42);
    /// }
    /// ```
    ///
    /// # Safety
    ///
    /// If the element whose ID is passed to this method has been removed, then this *may cause
    /// undefined behavior*.
    ///
    /// The given ID must have been created by this store, see the
    /// [struct-level documentation](RwStore).
    pub unsafe fn get_mut_unchecked(&mut self, id: Id) -> &mut Element {
        self.assert_native_id(id);
        Block::<Element>::get_unchecked(id).as_mut()
    }

    /// Creates an iterator that visits each element still present in the store, yielding its ID and
    /// a mutable reference to it.
    ///
    /// The order in which this iterator traverses elements is unspecified.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::RwStore;
    /// let mut store = RwStore::new();
    /// let id = store.insert(42);
    /// let mut iter = store.iter_mut();
    /// assert_eq!(iter.next().unwrap().1, &mut 42);
    /// assert!(iter.next().is_none());
    /// ```
    pub fn iter_mut(&mut self) -> IterMut<Element> {
        IterMut::new(self)
    }

    /// Determines the touched and allocated capacity for this store, and returns them in that
    /// order. These should be regarded as hints, if the store is being accessed concurrently the
    /// actual capacity values may be larger (but not smaller) than those returned by this method.
    ///
    /// The touched capacity is equal to the largest number of elements ever contained in this store
    /// at the same time. The allocated capacity is the total number of element slots allocated for
    /// use with this store. Neither of these numbers ever decrease.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::RwStore;
    /// let store = RwStore::new();
    /// assert_eq!(store.capacity(), (0, 0));
    ///
    /// store.insert(42);
    /// let (touched, allocated) = store.capacity();
    /// assert_eq!(touched, 1);
    /// assert!(allocated >= 1);
    /// ```
    pub fn capacity(&self) -> (u32, u32) {
        let (mut total_touched, mut total_allocated) = (0, 0);

        for bucket in &self.buckets {
            let (touched, allocated) = bucket.capacity();
            total_touched += touched;
            total_allocated += allocated;
        }

        (total_touched, total_allocated)
    }

    /// Generates a pseudo-random bucket ID for element insertion. This will return different
    /// results on different threads and invocations.
    fn arbitrary_bucket_id(&self) -> u32 {
        // Here we use a linear congruential generator using thread local state

        thread_local! {
            static STATE: UnsafeCell<u32> = UnsafeCell::new(thread::current_thread_hash() as u32);
        }

        STATE.with(|state| unsafe {
            const MULTIPLIER: u32 = 1103515245;
            const CONSTANT: u32 = 12345;

            let state = &mut *state.get();
            *state = state.wrapping_mul(MULTIPLIER).wrapping_add(CONSTANT);
            *state % BUCKET_COUNT as u32
        })
    }

    fn assert_native_id(&self, id: Id) {
        debug_assert!(
            self.store_id == id.store_id(),
            "attempted to use an ID created with a different store"
        )
    }
}

impl<Element> Default for RwStore<Element> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Element> IntoIterator for RwStore<Element> {
    type Item = (Id, Element);
    type IntoIter = IntoIter<Element>;

    /// Creates a consuming iterator that visits each element still present in the store, yielding its
    /// ID and value.
    ///
    /// The order in which this iterator traverses elements is unspecified.
    fn into_iter(self) -> IntoIter<Element> {
        IntoIter::new(self)
    }
}

unsafe impl<Element: Send + Sync> Sync for RwStore<Element> {}

unsafe impl<Element: Send> Send for RwStore<Element> {}

impl<Element: UnwindSafe> UnwindSafe for RwStore<Element> {}

impl<Element: RefUnwindSafe> RefUnwindSafe for RwStore<Element> {}

struct Erasure {
    slot_address: NonNull<()>,
}

#[cfg(test)]
mod test {
    use std::ops::Deref;
    use std::panic::{RefUnwindSafe, UnwindSafe};

    use crate::Timeout::DontBlock;
    use crate::{BlockResult, Id, RwStore};

    #[test]
    fn insert_creates_disparate_ids() {
        let store = RwStore::new();
        let id_a = store.insert(42);
        let id_b = store.insert(42);

        assert_ne!(id_a.slot::<()>(), id_b.slot());
    }

    #[test]
    fn insert_reuses_space_after_removal() {
        let store = RwStore::new();

        let id_a = store.insert(42);
        store.remove(id_a).unwrap();

        let id_b = store.insert(42);

        assert_eq!(id_a.slot::<()>(), id_b.slot());
    }

    #[test]
    fn insert_reuses_space_after_locked_removal() {
        let store = RwStore::new();

        let id_a = store.insert(42);
        store.remove_locked(store.write(id_a).unwrap());

        let id_b = store.insert(42);

        assert_eq!(id_a.slot::<()>(), id_b.slot());
    }

    #[test]
    fn insert_doesnt_reuse_id_ordinals() {
        let store = RwStore::new();

        let id_a = store.insert(42);
        store.remove(id_a).unwrap();

        let id_b = store.insert(42);

        assert_ne!(id_a.ordinal(), id_b.ordinal());
    }

    #[test]
    fn remove_returns_the_element() {
        gen_remove_returns_the_element(|store, id| store.remove(id));
    }

    #[test]
    fn remove_returns_none_after_removal() {
        gen_remove_returns_none_after_removal(|store, id| store.remove(id));
    }

    #[test]
    fn remove_with_timeout_returns_the_element() {
        gen_remove_returns_the_element(|store, id| {
            store.remove_with_timeout(id, DontBlock).unwrap()
        });
    }

    #[test]
    fn remove_with_timeout_returns_none_after_removal() {
        gen_remove_returns_none_after_removal(|store, id| {
            store.remove_with_timeout(id, DontBlock).unwrap()
        });
    }

    #[test]
    fn remove_with_timeout_fails_when_read_locked() {
        let store = RwStore::new();
        let id = store.insert(42);

        let _lock = store.read(id).unwrap();

        assert!(store.remove_with_timeout(id, DontBlock).is_err());
    }

    #[test]
    fn remove_with_timeout_fails_when_write_locked() {
        let store = RwStore::new();
        let id = store.insert(42);

        let _lock = store.write(id).unwrap();

        assert!(store.remove_with_timeout(id, DontBlock).is_err());
    }

    fn gen_remove_returns_the_element(remover: impl Fn(&RwStore<u32>, Id) -> Option<u32>) {
        let store = RwStore::new();
        let id = store.insert(42);

        assert_eq!(remover(&store, id), Some(42));
    }

    fn gen_remove_returns_none_after_removal(remover: impl Fn(&RwStore<u32>, Id) -> Option<u32>) {
        let store = RwStore::new();
        let id = store.insert(42);
        store.remove(id);

        assert_eq!(remover(&store, id), None);
    }

    #[test]
    fn remove_locked_returns_the_element() {
        let store = RwStore::new();
        let id = store.insert(42);

        let lock = store.write(id).unwrap();

        assert_eq!(store.remove_locked(lock), 42);
    }

    #[test]
    fn remove_locked_removes_the_element() {
        let store = RwStore::new();
        let id = store.insert(42);

        let lock = store.write(id).unwrap();
        store.remove_locked(lock);

        assert!(store.read(id).is_none());
    }

    #[test]
    fn read_references_the_element() {
        access_references_the_element::<ReadOperation>();
    }

    #[test]
    fn write_references_the_element() {
        access_references_the_element::<WriteOperation>();
    }

    #[test]
    fn read_with_timeout_references_the_element() {
        access_references_the_element::<ReadWithTimeoutOperation>();
    }

    #[test]
    fn write_with_timeout_references_the_element() {
        access_references_the_element::<WriteWithTimeoutOperation>();
    }

    fn access_references_the_element<A: AccessOperation>() {
        let store = RwStore::new();
        let id = store.insert(42);

        let lock = A::access(&store, id).unwrap().unwrap();

        assert_eq!(**lock, 42);
    }

    #[test]
    fn read_fails_returns_none_removal() {
        access_returns_none_after_removal::<ReadOperation>();
    }

    #[test]
    fn write_fails_returns_none_removal() {
        access_returns_none_after_removal::<WriteOperation>();
    }

    #[test]
    fn read_with_timeout_returns_none_after_removal() {
        access_returns_none_after_removal::<ReadWithTimeoutOperation>();
    }

    #[test]
    fn write_with_timeout_returns_none_after_removal() {
        access_returns_none_after_removal::<WriteWithTimeoutOperation>();
    }

    fn access_returns_none_after_removal<A: AccessOperation>() {
        let store = RwStore::new();
        let id = store.insert(42);

        store.remove(id).unwrap();

        assert!(A::access(&store, id).unwrap().is_none());
    }

    #[test]
    fn write_with_timeout_fails_when_write_locked() {
        access_fails_when_locked::<WriteOperation, WriteWithTimeoutOperation>();
    }

    #[test]
    fn write_with_timeout_fails_when_read_locked() {
        access_fails_when_locked::<ReadOperation, WriteWithTimeoutOperation>();
    }

    #[test]
    fn read_with_timeout_fails_when_write_locked() {
        access_fails_when_locked::<WriteOperation, ReadWithTimeoutOperation>();
    }

    fn access_fails_when_locked<Lock: AccessOperation, A: AccessOperation>() {
        let store = RwStore::new();
        let id = store.insert(42);

        let _lock = Lock::access(&store, id);

        assert!(A::access(&store, id).is_err());
    }

    #[test]
    fn read_succeeds_when_read_locked() {
        access_succeeds_when_locked::<ReadOperation, ReadOperation>();
    }

    #[test]
    fn read_with_timeout_succeeds_when_read_locked() {
        access_succeeds_when_locked::<ReadWithTimeoutOperation, ReadOperation>();
    }

    fn access_succeeds_when_locked<Lock: AccessOperation, A: AccessOperation>() {
        let store = RwStore::new();
        let id = store.insert(42);

        let _lock = Lock::access(&store, id);

        assert!(A::access(&store, id).is_ok());
    }

    #[test]
    fn get_mut_references_the_element() {
        let mut store = RwStore::new();
        let id = store.insert(42);

        assert_eq!(store.get_mut(id), Some(&mut 42));
    }

    #[test]
    fn get_mut_returns_none_after_removal() {
        let mut store = RwStore::new();
        let id = store.insert(42);

        store.remove(id).unwrap();

        assert_eq!(store.get_mut(id), None);
    }

    #[test]
    fn get_mut_unchecked_references_the_element() {
        let mut store = RwStore::new();
        let id = store.insert(42);

        unsafe {
            assert_eq!(store.get_mut_unchecked(id), &mut 42);
        }
    }

    #[test]
    fn capacity_returns_zeroes_initially() {
        let store = RwStore::<u32>::new();
        assert_eq!(store.capacity(), (0, 0));
    }

    #[test]
    fn capacity_increases_on_first_insertion() {
        let store = RwStore::<u32>::new();
        store.insert(42);
        assert_eq!(store.capacity().0, 1);
        assert!(store.capacity().1 >= 1);
    }

    #[test]
    fn capacity_increases_on_second_insertion() {
        let store = RwStore::<u32>::new();
        store.insert(42);
        store.insert(42);
        assert_eq!(store.capacity().0, 2);
        assert!(store.capacity().1 >= 2);
    }

    #[test]
    fn capacity_doesnt_decrease_after_removal() {
        let store = RwStore::<u32>::new();
        let id = store.insert(42);
        store.remove(id).unwrap();
        assert_eq!(store.capacity().0, 1);
        assert!(store.capacity().1 >= 1);
    }

    #[test]
    fn capacity_doesnt_increase_on_insertion_after_removal() {
        let store = RwStore::<u32>::new();
        let id = store.insert(42);
        store.remove(id).unwrap();
        store.insert(42);
        assert_eq!(store.capacity().0, 1);
        assert!(store.capacity().1 >= 1);
    }

    #[test]
    fn capacity_doesnt_increase_on_insertion_after_locked_removal() {
        let store = RwStore::<u32>::new();
        let id = store.insert(42);
        store.remove_locked(store.write(id).unwrap());
        store.insert(42);
        assert_eq!(store.capacity().0, 1);
        assert!(store.capacity().1 >= 1);
    }

    #[test]
    fn implements_sync() {
        let store = RwStore::<u32>::new();
        &store as &dyn Sync;
    }

    #[test]
    fn implements_send() {
        let store = RwStore::<u32>::new();
        &store as &dyn Send;
    }

    #[test]
    fn implements_unwind_safe() {
        let store = RwStore::<u32>::new();
        &store as &dyn UnwindSafe;
    }

    #[test]
    fn implements_ref_unwind_safe() {
        let store = RwStore::<u32>::new();
        &store as &dyn RefUnwindSafe;
    }

    trait AccessOperation {
        fn access<'a>(
            store: &'a RwStore<u32>,
            id: Id,
        ) -> BlockResult<Option<Box<dyn Deref<Target = u32> + 'a>>>;
    }

    struct ReadOperation;

    impl AccessOperation for ReadOperation {
        fn access<'a>(
            store: &'a RwStore<u32>,
            id: Id,
        ) -> BlockResult<Option<Box<dyn Deref<Target = u32> + 'a>>> {
            let result = store
                .read(id)
                .map(|lock| Box::new(lock) as Box<dyn Deref<Target = u32>>);

            Ok(result)
        }
    }

    struct WriteOperation;

    impl AccessOperation for WriteOperation {
        fn access<'a>(
            store: &'a RwStore<u32>,
            id: Id,
        ) -> BlockResult<Option<Box<dyn Deref<Target = u32> + 'a>>> {
            let result = store
                .write(id)
                .map(|lock| Box::new(lock) as Box<dyn Deref<Target = u32>>);

            Ok(result)
        }
    }

    struct ReadWithTimeoutOperation;

    impl AccessOperation for ReadWithTimeoutOperation {
        fn access<'a>(
            store: &'a RwStore<u32>,
            id: Id,
        ) -> BlockResult<Option<Box<dyn Deref<Target = u32> + 'a>>> {
            store
                .read_with_timeout(id, DontBlock)
                .map(|result| result.map(|lock| Box::new(lock) as Box<dyn Deref<Target = u32>>))
        }
    }

    struct WriteWithTimeoutOperation;

    impl AccessOperation for WriteWithTimeoutOperation {
        fn access<'a>(
            store: &'a RwStore<u32>,
            id: Id,
        ) -> BlockResult<Option<Box<dyn Deref<Target = u32> + 'a>>> {
            store
                .write_with_timeout(id, DontBlock)
                .map(|result| result.map(|lock| Box::new(lock) as Box<dyn Deref<Target = u32>>))
        }
    }
}
