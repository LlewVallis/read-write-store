//! A concurrent, unordered collection where each element has an internally generated ID and a
//! read-write lock.
//!
//! See the [struct-level documentation](RwStore).

#![feature(option_result_unwrap_unchecked)]
#![feature(ptr_metadata)]
#![warn(missing_docs)]

use std::mem;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::ptr::NonNull;

use crate::block::Block;
pub use crate::id::*;
use crate::iter::{IntoIter, IterMut};
use crate::lock::{ReadLock, WriteLock};
use crate::rwstore_id::RwStoreId;
use crate::timeout::{BlockResult, Timeout};
pub use crate::timeout::*;
use crate::util::sync::atomic::{AtomicU32, Ordering};
use crate::util::sync::concurrent_queue::ConcurrentQueue;
use crate::util::sync::rwlock::RwLock;

mod block;
mod header;
pub mod id;
pub mod iter;
pub mod lock;
#[cfg(all(test, loom))]
mod loom;
mod rwstore_id;
pub mod timeout;
mod util;

const INITIAL_BLOCK_SIZE: u32 = 16;
const RESIZE_FACTOR: u32 = 2;
const RESERVED_ID: u32 = 0;

/// A concurrent, unordered collection where each element has an internally generated ID and a
/// read-write lock.
///
/// A store has O(1) time complexity for insertion, removal and lookups, although memory
/// allocations triggered by insertion may cause a performance spike.
///
/// # Example
///
/// ```
/// # use read_write_store::RwStore;
/// # use read_write_store::timeout::Timeout::DontBlock;
/// // Note that we only need an immutable reference to the store
/// let store = RwStore::new();
///
/// // Inserting an element yields an ID
/// let id = store.insert(42);
///
/// {
///     // You can read the element's value using that ID
///     let read_lock = store.read(id).unwrap();
///     assert_eq!(*read_lock, 42);
///
///     // Concurrent reads are possible
///     assert!(store.read_with_timeout(id, DontBlock).is_ok());
///     // But reading and writing at the same time won't work
///     assert!(store.write_with_timeout(id, DontBlock).is_err());
/// }
///
/// {
///     // You can also acquire a write lock using an ID
///     let mut write_lock = store.write(id).unwrap();
///     *write_lock = 24;
///     assert_eq!(*write_lock, 24);
///
///     // Predictably, you cannot have multiple writers
///     assert!(store.write_with_timeout(id, DontBlock).is_err());
/// }
///
/// // Elements can of course be removed using their ID
/// assert_eq!(store.remove(id), Some(24));
///
/// // Now if we try to read using the ID, it will fail gracefully
/// assert!(store.read(id).is_none());
/// ```
///
/// # Allocation behavior
///
/// * An empty store does not require any allocation.
/// * Once an element is inserted, a fixed size block of element slots will be allocated.
/// * When the store has reached capacity, a block double the size of the last one will be allocated
///   and used concurrently with all previously allocated blocks.
/// * All removed elements whose memory is ready for reuse are tracked, so removing an element may
///   cause a small allocation.
///
/// # Caveats
///
/// * When a block of elements is allocated internally, it won't be deallocated until the store is
///   dropped. This allows the element ID's to contain a pointer to the element that is valid for
///   the lifetime of the store.
/// * Every element has a 8 bytes of overhead.
/// * No facilities are currently provided for parallel iteration. Although this could be done, it
///   would mean an element whose ID is not shared between threads would no longer be guaranteed to
///   be accessible only from its inserting thread.
/// * For performance reasons, no facilities are provided for querying the size in elements of the
///   store.
/// * A maximum of 2<sup>32</sup> - 1 elements can ever be inserted into any given store, including
///   elements that have been removed. Breaking this invariant will panic when debug assertions are
///   enabled, but may cause undefined behavior when they are disabled (i.e. in release mode).
///
/// # Safety
///
/// Each element inserted into a store is given an *internal* unique ID. This begs the question,
/// what happens when an ID from one store is used with another? When debug assertions are enabled,
/// a best effort attempt is made to panic when using an ID from a different store, but when they
/// are disabled (i.e. in release mode), or if this best effort fails, this
/// *may cause undefined behavior*.
///
/// Similarly, inserting more than 2<sup>32</sup> - 1 elements, including removed elements, into a
/// store will panic when debug assertions are enabled but *may cause undefined behavior* when they
/// are not.
pub struct RwStore<Element> {
    store_id: RwStoreId,
    // Should only be increased, should not be read from
    next_id: AtomicU32,
    blocks: BlockList<Element>,
    // Contains slots ready for insertion
    erasures: ConcurrentQueue<NonNull<()>>,
}

impl<Element> RwStore<Element> {
    /// Creates a new, empty store.
    pub fn new() -> Self {
        let store_id = RwStoreId::generate();

        Self {
            store_id,
            next_id: AtomicU32::new(RESERVED_ID + 1),
            blocks: BlockList::new(store_id),
            erasures: ConcurrentQueue::new(),
        }
    }

    /// Inserts an element into the store, returning it's generated unique ID. The returned ID can be
    /// used to subsequently read, modify or remove the element.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::RwStore;
    /// let store = RwStore::new();
    /// let id = store.insert(42);
    /// ```
    ///
    /// # Safety
    ///
    /// This should not be called more than 2<sup>32</sup> - 1 times on any given store. When debug
    /// assertions are enabled, doing so will panic, but when they are disabled (i.e. in release
    /// mode), doing so *may cause undefined behavior*.
    pub fn insert(&self, element: Element) -> Id {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        debug_assert!(id != RESERVED_ID, "element IDs have been exhausted");

        let slot_address = self.next_insert_location();
        unsafe { Block::insert(id, slot_address, element, self.store_id) }
    }

    fn next_insert_location(&self) -> NonNull<()> {
        if let Some(location) = self.erasures.pop() {
            return location;
        }

        self.blocks.next_insert_location()
    }

    /// Removes an element from the store using its ID if it has not already been removed. Returns the
    /// element if it was present.
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

    /// Removes an element from the store using its ID if it has not already been removed. Returns the
    /// element if it was present.
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
            if let Some(value) = Block::<Element>::remove(id, timeout)? {
                self.erasures.push(id.slot_address);
                Ok(Some(value))
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
    /// The given lock must have been acquired from one of the locking methods on this store. Using a
    /// lock from another store will panic when debug assertions are enabled, but *may cause undefined
    /// behavior* when they are disabled (i.e. in release mode).
    pub fn remove_locked(&self, lock: WriteLock<Element>) -> Element {
        let id = lock.forget();

        self.assert_native_id(id);

        unsafe { Block::<Element>::remove_locked(id) }
    }

    /// Acquires a read lock on an element given its ID, if it is still present in the store.
    ///
    /// If a write lock is held on the element, this will block until it is released.
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

    /// Determines the touched and allocated capacity for this store, and returns them in that order.
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
        let block_list = self.blocks.inner.read();

        let head_size = block_list
            .head
            .as_ref()
            .map(|block| block.len())
            .unwrap_or(0);

        let used = block_list.next_unused_slot.load(Ordering::Acquire);

        mem::drop(block_list);

        let allocated_capacity = head_size_to_total_size(head_size);
        let touched_capacity = head_size_to_total_size(head_size) - (head_size - used);

        (touched_capacity, allocated_capacity)
    }

    fn assert_native_id(&self, id: Id) {
        debug_assert!(
            self.store_id == id.store_id,
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

struct BlockList<Element> {
    inner: RwLock<BlockListInner<Element>>,
}

impl<Element> BlockList<Element> {
    pub fn new(store_id: RwStoreId) -> Self {
        Self {
            inner: RwLock::new(BlockListInner::new(store_id)),
        }
    }

    pub fn next_insert_location(&self) -> NonNull<()> {
        let inner_read = self.inner.read();

        if let Some(location) = inner_read.next_insert_location() {
            return location;
        }

        mem::drop(inner_read);
        self.next_insert_location_slow()
    }

    #[inline(never)]
    fn next_insert_location_slow(&self) -> NonNull<()> {
        let mut inner_write = self.inner.write();

        if let Some(location) = inner_write.next_insert_location() {
            return location;
        }

        inner_write.expand();
        inner_write.next_insert_location().unwrap()
    }
}

struct BlockListInner<Element> {
    head: Option<Pin<Box<Block<Element>>>>,
    // This can only increase while read locked, but may be reset when write locked
    next_unused_slot: AtomicU32,
    store_id: RwStoreId,
}

impl<Element> BlockListInner<Element> {
    pub fn new(store_id: RwStoreId) -> Self {
        Self {
            head: None,
            next_unused_slot: AtomicU32::new(0),
            store_id,
        }
    }

    pub fn next_insert_location(&self) -> Option<NonNull<()>> {
        if let Some(head) = &self.head {
            let slot = self.next_unused_slot.fetch_add(1, Ordering::AcqRel);

            if slot < head.len() {
                unsafe { Some(head.slot_address(slot)) }
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn expand(&mut self) {
        let block_size = if let Some(head) = &self.head {
            head.len() * RESIZE_FACTOR
        } else {
            INITIAL_BLOCK_SIZE
        };

        let old_head = self.head.take();
        let new_head = Block::new(block_size, old_head, self.store_id);

        self.head = Some(new_head);
        self.next_unused_slot.store_directly(0);
    }
}

fn head_size_to_total_size(head_size: u32) -> u32 {
    let mut total = 0;
    let mut considered_block_size = INITIAL_BLOCK_SIZE;
    while considered_block_size <= head_size {
        total += considered_block_size;
        considered_block_size *= RESIZE_FACTOR;
    }

    total
}

#[cfg(test)]
mod test {
    use std::ops::Deref;
    use std::panic::{RefUnwindSafe, UnwindSafe};

    use crate::{BlockResult, Id, RwStore};
    use crate::Timeout::DontBlock;

    #[test]
    fn insert_creates_disparate_ids() {
        let store = RwStore::new();
        let id_a = store.insert(42);
        let id_b = store.insert(42);

        assert_ne!(id_a.number, id_b.number);
        assert_ne!(id_a.slot_address, id_b.slot_address);
    }

    #[test]
    fn insert_reuses_space_optimally() {
        let store = RwStore::new();

        let id_a = store.insert(42);
        store.remove(id_a).unwrap();

        let id_b = store.insert(42);

        assert_eq!(id_a.slot_address, id_b.slot_address);
    }

    #[test]
    fn insert_doesnt_reuse_id_numbers() {
        let store = RwStore::new();

        let id_a = store.insert(42);
        store.remove(id_a).unwrap();

        let id_b = store.insert(42);

        assert_ne!(id_a.number, id_b.number);
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
