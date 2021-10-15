use std::mem;
use std::pin::Pin;
use std::ptr::NonNull;

use crate::block::Block;
use crate::rwstore_id::RwStoreId;
use crate::util::sync::atomic::{AtomicU32, Ordering};
use crate::util::sync::rwlock::RwLock;

const INITIAL_BLOCK_SIZE: u32 = 1;
const RESIZE_FACTOR: u32 = 2;

pub struct Bucket<Element> {
    pub inner: RwLock<BucketInner<Element>>,
}

impl<Element> Bucket<Element> {
    pub fn new(store_id: RwStoreId) -> Self {
        Self {
            inner: RwLock::new(BucketInner::new(store_id)),
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

    pub fn capacity(&self) -> (u32, u32) {
        let block_list = self.inner.read();

        let head_size = block_list
            .head
            .as_ref()
            .map(|block| block.len())
            .unwrap_or(0);

        let used = block_list.next_unused_slot.load(Ordering::Relaxed);

        mem::drop(block_list);

        let allocated_capacity = head_size_to_total_size(head_size);
        let touched_capacity = head_size_to_total_size(head_size) - (head_size - used);

        (touched_capacity, allocated_capacity)
    }
}

pub struct BucketInner<Element> {
    pub head: Option<Pin<Box<Block<Element>>>>,
    // This can only increase while read locked, but may be reset when write locked
    pub next_unused_slot: AtomicU32,
    pub store_id: RwStoreId,
}

impl<Element> BucketInner<Element> {
    pub fn new(store_id: RwStoreId) -> Self {
        Self {
            head: None,
            next_unused_slot: AtomicU32::new(0),
            store_id,
        }
    }

    pub fn next_insert_location(&self) -> Option<NonNull<()>> {
        if let Some(head) = &self.head {
            let slot = self.next_unused_slot.fetch_add(1, Ordering::Relaxed);

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

pub fn head_size_to_total_size(head_size: u32) -> u32 {
    let mut total = 0;
    let mut considered_block_size = INITIAL_BLOCK_SIZE;
    while considered_block_size <= head_size {
        total += considered_block_size;
        considered_block_size *= RESIZE_FACTOR;
    }

    total
}
