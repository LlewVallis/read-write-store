use std::alloc;
use std::alloc::Layout;
use std::cell::UnsafeCell;
use std::marker::PhantomPinned;
use std::mem;
use std::pin::Pin;
use std::ptr;
use std::ptr::NonNull;

use crate::header::Header;
use crate::header::RESERVED_ID;
use crate::id::Id;
use crate::rwstore_id::RwStoreId;
use crate::util::debug_check::{DebugCheckedMaybeUninit, IndexDebugChecked, UnwrapDebugChecked};
use crate::BlockResult;
use crate::Timeout;

type Next<Element> = Option<Pin<Box<Block<Element>>>>;

/// An array of slots, each containing a header and element. Also contains a pointer to the block
/// which was most recently allocated before this one. After a block is created, it cannot be moved
/// or dropped until the entire store is dropped.
// repr(c) so we can initialize it manually
#[repr(C)]
pub struct Block<Element> {
    header: BlockHeader<Element>,
    slots: [Slot<Element>],
}

impl<Element> Block<Element> {
    pub fn new(size: u32, next: Option<Pin<Box<Self>>>, store_id: RwStoreId) -> Pin<Box<Self>> {
        debug_assert!(size != 0, "size cannot be zero");
        debug_assert!(size.is_power_of_two(), "size must be a power of two");

        let header_size = mem::size_of::<BlockHeader<Element>>();
        let header_align = mem::align_of::<BlockHeader<Element>>();

        let slot_size = mem::size_of::<Slot<Element>>();
        let slots_size = slot_size * size as usize;
        let slots_align = mem::align_of::<Slot<Element>>();
        let slots_offset = Self::next_multiple_of(header_size, slots_align);

        let block_align = header_align.max(slots_align);
        let block_size = Self::next_multiple_of(slots_offset + slots_size, block_align);

        unsafe {
            let layout = Layout::from_size_align(block_size, block_align).unwrap_debug_checked();
            let bytes_ptr = alloc::alloc(layout);

            let header_ptr = bytes_ptr as *mut BlockHeader<Element>;
            header_ptr.write(BlockHeader::new(next, store_id));

            for i in 0..size {
                let slot_ptr =
                    bytes_ptr.add(slots_offset).add(slot_size * i as usize) as *mut Slot<Element>;
                slot_ptr.write(Slot::new());
            }

            let block_ptr = ptr::from_raw_parts_mut::<Self>(bytes_ptr as *mut (), size as usize);
            Pin::from(Box::from_raw(block_ptr))
        }
    }

    fn next_multiple_of(number: usize, multiple: usize) -> usize {
        if number % multiple == 0 {
            number
        } else {
            let excess = number % multiple;
            let padding = multiple - excess;
            number + padding
        }
    }

    pub fn len(&self) -> u32 {
        self.slots.len() as u32
    }

    pub unsafe fn slot_address(&self, slot: u32) -> NonNull<()> {
        let slot = self.scramble_slot(slot);
        NonNull::from(self.slots.get_debug_checked(slot as usize)).cast()
    }

    pub unsafe fn insert(
        id: u32,
        bucket_id: u32,
        slot_address: NonNull<()>,
        element: Element,
        store_id: RwStoreId,
    ) -> Id {
        let slot = slot_address.cast::<Slot<Element>>().as_ref();

        slot.value
            .get()
            .write(DebugCheckedMaybeUninit::new(element));

        slot.header.insert(id);

        Id::new(id, bucket_id, slot, store_id)
    }

    pub unsafe fn remove(id: Id, timeout: Timeout) -> BlockResult<Option<Element>> {
        let slot = Self::slot_from_id(id);

        if slot.header.remove(id.ordinal(), timeout)? {
            let element = slot.value.get().read().assume_init();
            Ok(Some(element))
        } else {
            Ok(None)
        }
    }

    pub unsafe fn remove_locked(id: Id) -> Element {
        let slot = Self::slot_from_id(id);

        let element = slot.value.get().read().assume_init();
        slot.header.remove_locked(id.ordinal());
        element
    }

    pub unsafe fn lock_read(id: Id, timeout: Timeout) -> BlockResult<bool> {
        let slot = Self::slot_from_id(id);
        slot.header.lock_read(id.ordinal(), timeout)
    }

    pub unsafe fn unlock_read(id: Id) {
        let slot = Self::slot_from_id(id);
        slot.header.unlock_read(id.ordinal())
    }

    pub unsafe fn lock_write(id: Id, timeout: Timeout) -> BlockResult<bool> {
        let slot = Self::slot_from_id(id);
        slot.header.lock_write(id.ordinal(), timeout)
    }

    pub unsafe fn unlock_write(id: Id) {
        let slot = Self::slot_from_id(id);
        slot.header.unlock_write(id.ordinal())
    }

    /// Gets a pointer to the contents of the slot referenced in the given ID. The ID's number must
    /// match the ID number stored in the slot.
    pub unsafe fn get_unchecked(id: Id) -> NonNull<Element> {
        let slot = Self::slot_from_id(id);
        let contents = &*slot.value.get();
        contents.contents_ptr()
    }

    /// Gets a pointer to the contents of the slot referenced in the given ID if it's ID number
    /// matches the slot's ID number. The calling thread must have exclusive, externally
    /// synchronized ownership of the slot.
    pub unsafe fn get_exclusive(id: Id) -> Option<NonNull<Element>> {
        let slot = Self::slot_from_id_mut(id);

        if slot.header.id() == id.ordinal() {
            let contents = &*slot.value.get();
            Some(contents.contents_ptr())
        } else {
            None
        }
    }

    pub unsafe fn take(&mut self, slot: u32, bucket_id: u32) -> Option<(Id, Element)> {
        let slot = self.scramble_slot(slot);
        let slot = self.slots.get_debug_checked_mut(slot as usize);

        if let Some(id) = slot.header.reset() {
            let id = Id::new(id, bucket_id, slot, self.header.store_id);
            let element = slot.value.get().read().assume_init();
            Some((id, element))
        } else {
            None
        }
    }

    pub unsafe fn slot_contents<'a>(
        &mut self,
        slot: u32,
        bucket_id: u32,
    ) -> Option<(Id, &'a mut Element)> {
        let slot = self.scramble_slot(slot);
        let slot = self.slots.get_debug_checked_mut(slot as usize);

        let id = slot.header.id();

        if id != RESERVED_ID {
            let id = Id::new(id, bucket_id, slot, self.header.store_id);

            let element_contents = (&*slot.value.get()).contents_ptr();
            let element = &mut *element_contents.as_ptr();

            Some((id, element))
        } else {
            None
        }
    }

    pub fn into_next(mut self: Pin<Box<Self>>) -> Option<Pin<Box<Self>>> {
        unsafe { self.as_mut().get_unchecked_mut().header.next.take() }
    }

    pub fn next_mut(&mut self) -> Option<&mut Pin<Box<Self>>> {
        self.header.next.as_mut()
    }

    unsafe fn slot_from_id<'a>(id: Id) -> &'a Slot<Element> {
        id.slot().as_ref()
    }

    unsafe fn slot_from_id_mut<'a>(id: Id) -> &'a mut Slot<Element> {
        id.slot().as_mut()
    }

    /// Takes a logical slot and returns a physical slot which has been mapped pseudo-randomly to
    /// help with false sharing.
    fn scramble_slot(&self, slot: u32) -> u32 {
        // Multiplying by this factor and then taking it modulo the length is bijective if our
        // factor is coprime with our length. Since all block lengths are powers of two, 3 will
        // always be coprime with the length
        const FACTOR: u32 = 3;

        let unbounded = slot as u64 * FACTOR as u64;

        // Since len is a power of two, we can create a mask that truncates to the length by
        // removing one. This allows us to quickly perform a modulo
        let bounded = unbounded as u32 & (self.len() - 1);

        bounded
    }
}

struct BlockHeader<Element> {
    next: Next<Element>,
    store_id: RwStoreId,
}

impl<Element> BlockHeader<Element> {
    pub fn new(next: Next<Element>, store_id: RwStoreId) -> Self {
        Self { next, store_id }
    }
}

struct Slot<Element> {
    header: Header,
    value: UnsafeCell<DebugCheckedMaybeUninit<Element>>,
    _marker: PhantomPinned,
}

impl<Element> Slot<Element> {
    pub fn new() -> Self {
        Self {
            header: Header::new(),
            value: UnsafeCell::new(DebugCheckedMaybeUninit::uninit()),
            _marker: PhantomPinned::default(),
        }
    }
}
impl<Element> Drop for Slot<Element> {
    fn drop(&mut self) {
        if self.header.is_occupied() {
            unsafe { self.value.get().cast::<Element>().drop_in_place() }
        }
    }
}

#[cfg(test)]
mod test {
    use std::cell::Cell;
    use std::pin::Pin;
    use std::rc::Rc;

    use crate::block::{Block, Slot};
    use crate::id::Id;
    use crate::rwstore_id::RwStoreId;
    use crate::Timeout::DontBlock;

    #[test]
    fn len_returns_the_size() {
        let block = Block::<()>::new(32, None, store_id());
        assert_eq!(block.len(), 32);
    }

    #[test]
    fn insert_creates_an_appropriate_id() {
        unsafe {
            let store_id = store_id();
            let block = Block::<u32>::new(1, None, store_id);
            let slot = block.slot_address(0);
            let id = Block::insert(42, 24, slot, 12, store_id);

            assert_eq!(id.ordinal(), 42);
            assert_eq!(id.bucket_id(), 24);
        }
    }

    #[test]
    fn removal_fails_on_an_empty_slot() {
        unsafe {
            let store_id = store_id();
            let block = Block::<u32>::new(1, None, store_id);

            let id = {
                let slot = block.slot_address(0).cast::<Slot<u32>>().as_ref();
                Id::new(42, 24, slot, store_id)
            };

            assert_eq!(Block::<u32>::remove(id, DontBlock), Ok(None));
        }
    }

    #[test]
    fn removal_succeeds_when_id_matches() {
        unsafe {
            let (_block, id) = singleton_block();
            assert_eq!(Block::<u32>::remove(id, DontBlock), Ok(Some(24)));
        }
    }

    #[test]
    fn removal_fails_when_id_doesnt_match() {
        unsafe {
            let (_block, id) = singleton_block();
            let id = create_mismatching_id(id);
            assert_eq!(Block::<u32>::remove(id, DontBlock), Ok(None));
        }
    }

    #[test]
    fn lock_read_succeeds_when_id_matches() {
        unsafe {
            let (_block, id) = singleton_block();
            assert_eq!(Block::<u32>::lock_read(id, DontBlock), Ok(true));
            Block::<u32>::unlock_read(id);
        }
    }

    #[test]
    fn lock_read_fails_when_id_doesnt_match() {
        unsafe {
            let (_block, id) = singleton_block();
            let id = create_mismatching_id(id);
            assert_eq!(Block::<u32>::lock_read(id, DontBlock), Ok(false));
        }
    }

    fn create_mismatching_id(id: Id) -> Id {
        unsafe {
            let slot = id.slot::<Slot<u32>>().as_ref();
            Id::new(id.ordinal() + 1, id.bucket_id(), slot, id.store_id())
        }
    }

    #[test]
    fn removal_drops_the_element() {
        let state = Rc::new(Cell::new(0));

        #[derive(Debug)]
        struct Element {
            state: Rc<Cell<u32>>,
        }

        impl Drop for Element {
            fn drop(&mut self) {
                self.state.set(42);
            }
        }

        unsafe {
            let store_id = store_id();
            let block = Block::<Element>::new(1, None, store_id);
            let slot = block.slot_address(0);

            let element = Element {
                state: state.clone(),
            };

            let id = Block::insert(42, 24, slot, element, store_id);
            Block::<Element>::remove(id, DontBlock).unwrap();

            assert_eq!(state.get(), 42);
        }
    }

    #[test]
    fn locked_removal_drops_the_element() {
        let state = Rc::new(Cell::new(0));

        #[derive(Debug)]
        struct Element {
            state: Rc<Cell<u32>>,
        }

        impl Drop for Element {
            fn drop(&mut self) {
                self.state.set(42);
            }
        }

        unsafe {
            let store_id = store_id();
            let block = Block::<Element>::new(1, None, store_id);
            let slot = block.slot_address(0);

            let element = Element {
                state: state.clone(),
            };

            let id = Block::insert(42, 24, slot, element, store_id);

            Block::<Element>::lock_write(id, DontBlock).unwrap();
            Block::<Element>::remove_locked(id);

            assert_eq!(state.get(), 42);
        }
    }

    #[test]
    fn dropping_the_block_drops_all_elements() {
        let state = Rc::new(Cell::new(0));

        #[derive(Debug)]
        struct Element {
            state: Rc<Cell<u32>>,
        }

        impl Drop for Element {
            fn drop(&mut self) {
                self.state.set(42);
            }
        }

        unsafe {
            let store_id = store_id();
            let block = Block::<Element>::new(1, None, store_id);
            let slot = block.slot_address(0);

            let element = Element {
                state: state.clone(),
            };

            Block::insert(42, 24, slot, element, store_id);
        }

        assert_eq!(state.get(), 42);
    }

    unsafe fn singleton_block() -> (Pin<Box<Block<u32>>>, Id) {
        let store_id = store_id();
        let block = Block::new(1, None, store_id);
        let slot = block.slot_address(0);
        let id = Block::insert(42, 12, slot, 24, store_id);
        (block, id)
    }

    fn store_id() -> RwStoreId {
        RwStoreId::generate()
    }
}
