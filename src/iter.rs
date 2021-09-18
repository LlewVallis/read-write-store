//! Iterator implementations for [RwStore](crate::RwStore).

use std::iter::FusedIterator;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::pin::Pin;

use crate::block::Block;
use crate::id::Id;
use crate::util::debug_check::UnwrapDebugChecked;
use crate::{head_size_to_total_size, RwStore};

/// An owned iterator over an [RwStore](crate::RwStore).
///
/// # Example
///
/// ```
/// # use read_write_store::RwStore;
/// let store = RwStore::new();
/// let id = store.insert(42);
/// let mut iter = store.into_iter();
/// assert_eq!(iter.next().unwrap().1, 42);
/// assert!(iter.next().is_none());
/// ```
pub struct IntoIter<Element> {
    iter: GenIter<OwnedBlockTracker<Element>>,
}

impl<Element> IntoIter<Element> {
    /// Creates an owned iterator over an [RwStore](crate::RwStore).
    pub fn new(store: RwStore<Element>) -> Self {
        let block_list = store.blocks.inner.into_inner();

        let last_slot = block_list.next_unused_slot.into_inner();
        let current_block = block_list.head.map(OwnedBlockTracker::new);

        Self {
            iter: GenIter::new(last_slot, current_block),
        }
    }
}

impl<Element> Iterator for IntoIter<Element> {
    type Item = (Id, Element);

    fn next(&mut self) -> Option<(Id, Element)> {
        self.iter.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl<Element> FusedIterator for IntoIter<Element> {}

unsafe impl<Element> Sync for IntoIter<Element> {}

unsafe impl<Element: Send> Send for IntoIter<Element> {}

impl<Element: UnwindSafe> UnwindSafe for IntoIter<Element> {}

impl<Element: RefUnwindSafe> RefUnwindSafe for IntoIter<Element> {}

/// A mutable iterator over an [RwStore](crate::RwStore).
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
pub struct IterMut<'a, Element> {
    iter: GenIter<MutBlockTracker<'a, Element>>,
}

impl<'a, Element> IterMut<'a, Element> {
    /// Creates a mutable iterator over an [RwStore](crate::RwStore).
    pub fn new(store: &'a mut RwStore<Element>) -> Self {
        let block_list = store.blocks.inner.get_mut();

        let last_slot = block_list.next_unused_slot.load_directly();
        let current_block = block_list.head.as_mut().map(MutBlockTracker::new);

        Self {
            iter: GenIter::new(last_slot, current_block),
        }
    }
}

impl<'a, Element> Iterator for IterMut<'a, Element> {
    type Item = (Id, &'a mut Element);

    fn next(&mut self) -> Option<(Id, &'a mut Element)> {
        self.iter.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl<'a, Element> FusedIterator for IterMut<'a, Element> {}

unsafe impl<'a, Element> Sync for IterMut<'a, Element> {}

unsafe impl<'a, Element: Send> Send for IterMut<'a, Element> {}

impl<'a, Element: UnwindSafe> UnwindSafe for IterMut<'a, Element> {}

impl<'a, Element: RefUnwindSafe> RefUnwindSafe for IterMut<'a, Element> {}

struct GenIter<Tracker: GenBlockTracker> {
    last_slot: u32,
    current_block: Option<Tracker>,
}

impl<Tracker: GenBlockTracker> GenIter<Tracker> {
    pub fn new(last_slot: u32, current_block: Option<Tracker>) -> Self {
        let max_last_slot = current_block.as_ref().map(|block| block.len()).unwrap_or(0);
        let last_slot = last_slot.min(max_last_slot);

        Self {
            last_slot,
            current_block,
        }
    }
}

impl<Tracker: GenBlockTracker> Iterator for GenIter<Tracker> {
    type Item = Tracker::Element;

    fn next(&mut self) -> Option<Tracker::Element> {
        unsafe {
            if self.last_slot == 0 {
                let old_block = self.current_block.take()?;

                let new_block = old_block.next();
                let new_block_size = new_block.as_ref().map(|block| block.len()).unwrap_or(0);

                self.current_block = new_block;
                self.last_slot = new_block_size;

                return self.next();
            }

            let current_block = self.current_block.as_mut().unwrap_debug_checked();
            self.last_slot -= 1;

            let contents = current_block.take(self.last_slot);
            match contents {
                Some(result) => Some(result),
                None => self.next(),
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let current_block_size = self
            .current_block
            .as_ref()
            .map(|block| block.len())
            .unwrap_or(0);

        let allocated_size = head_size_to_total_size(current_block_size);
        let touched_size = allocated_size - (current_block_size - self.last_slot);

        (0, Some(touched_size as usize))
    }
}

impl<Tracker: GenBlockTracker> FusedIterator for GenIter<Tracker> {}

trait GenBlockTracker: Sized {
    type Element;

    fn next(self) -> Option<Self>;

    unsafe fn take(&mut self, slot: u32) -> Option<Self::Element>;

    fn len(&self) -> u32;
}

struct OwnedBlockTracker<Element> {
    block: Pin<Box<Block<Element>>>,
}

impl<Element> OwnedBlockTracker<Element> {
    pub fn new(block: Pin<Box<Block<Element>>>) -> Self {
        Self { block }
    }
}

impl<Element> GenBlockTracker for OwnedBlockTracker<Element> {
    type Element = (Id, Element);

    fn next(self) -> Option<Self> {
        Block::into_next(self.block).map(OwnedBlockTracker::new)
    }

    unsafe fn take(&mut self, slot: u32) -> Option<(Id, Element)> {
        self.block.as_mut().get_unchecked_mut().take(slot)
    }

    fn len(&self) -> u32 {
        self.block.len()
    }
}

struct MutBlockTracker<'a, Element> {
    block: &'a mut Pin<Box<Block<Element>>>,
}

impl<'a, Element> MutBlockTracker<'a, Element> {
    pub fn new(block: &'a mut Pin<Box<Block<Element>>>) -> Self {
        Self { block }
    }
}

impl<'a, Element> GenBlockTracker for MutBlockTracker<'a, Element> {
    type Element = (Id, &'a mut Element);

    fn next(self) -> Option<Self> {
        unsafe {
            self.block
                .as_mut()
                .get_unchecked_mut()
                .next_mut()
                .map(MutBlockTracker::new)
        }
    }

    unsafe fn take(&mut self, slot: u32) -> Option<(Id, &'a mut Element)> {
        self.block.as_mut().get_unchecked_mut().slot_contents(slot)
    }

    fn len(&self) -> u32 {
        self.block.len()
    }
}

#[cfg(test)]
mod test {
    use std::panic::{RefUnwindSafe, UnwindSafe};

    use crate::RwStore;

    #[test]
    fn into_iter_has_correct_size() {
        iter_has_correct_size(|store| store.into_iter().count());
    }

    #[test]
    fn iter_mut_has_correct_size() {
        iter_has_correct_size(|mut store| store.iter_mut().count());
    }

    fn iter_has_correct_size(count: impl Fn(RwStore<u32>) -> usize) {
        let store = RwStore::new();
        for i in 0..42 {
            store.insert(i);
        }
        assert_eq!(count(store), 42);

        let store = RwStore::new();
        store.insert(42);
        assert_eq!(count(store), 1);

        let store = RwStore::new();
        assert_eq!(count(store), 0);
    }

    #[test]
    fn into_iter_has_correct_elements() {
        iter_has_correct_elements(|store| store.into_iter().map(|(_, value)| value).sum())
    }

    #[test]
    fn iter_mut_has_correct_elements() {
        iter_has_correct_elements(|mut store| store.iter_mut().map(|(_, value)| *value).sum())
    }

    fn iter_has_correct_elements(sum: impl Fn(RwStore<u32>) -> u32) {
        let store = RwStore::new();
        store.insert(1);
        store.insert(2);
        store.insert(4);
        assert_eq!(sum(store), 7);

        let store = RwStore::new();
        for i in 1..=42 {
            store.insert(i);
        }
        assert_eq!(sum(store), 903);

        let store = RwStore::new();
        assert_eq!(sum(store), 0);
    }

    #[test]
    fn into_iter_skips_removed_elements() {
        iter_skips_removed_elements(|store| store.into_iter().map(|(_, value)| value).sum())
    }

    #[test]
    fn iter_mut_skips_removed_elements() {
        iter_skips_removed_elements(|mut store| store.iter_mut().map(|(_, value)| *value).sum())
    }

    fn iter_skips_removed_elements(sum: impl FnOnce(RwStore<u32>) -> u32) {
        let store = RwStore::new();

        store.insert(1);
        let id = store.insert(2);
        store.insert(4);

        store.remove(id).unwrap();

        assert_eq!(sum(store), 5);
    }

    #[test]
    fn into_iter_implements_sync() {
        let store = RwStore::<*const ()>::new();
        let iter = store.into_iter();
        &iter as &dyn Sync;
    }

    #[test]
    fn iter_mut_implements_sync() {
        let mut store = RwStore::<*const ()>::new();
        let iter = store.iter_mut();
        &iter as &dyn Sync;
    }

    #[test]
    fn into_iter_implements_send() {
        let store = RwStore::<u32>::new();
        let iter = store.into_iter();
        &iter as &dyn Send;
    }

    #[test]
    fn iter_mut_implements_send() {
        let mut store = RwStore::<u32>::new();
        let iter = store.iter_mut();
        &iter as &dyn Send;
    }

    #[test]
    fn into_iter_implements_unwind_safe() {
        let store = RwStore::<u32>::new();
        let iter = store.into_iter();
        &iter as &dyn UnwindSafe;
    }

    #[test]
    fn iter_mut_implements_unwind_safe() {
        let mut store = RwStore::<u32>::new();
        let iter = store.iter_mut();
        &iter as &dyn UnwindSafe;
    }

    #[test]
    fn into_iter_implements_ref_unwind_safe() {
        let store = RwStore::<u32>::new();
        let iter = store.into_iter();
        &iter as &dyn RefUnwindSafe;
    }

    #[test]
    fn iter_mut_implements_ref_unwind_safe() {
        let mut store = RwStore::<u32>::new();
        let iter = store.iter_mut();
        &iter as &dyn RefUnwindSafe;
    }
}
