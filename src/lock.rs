//! RAII read and write locks for [RwStore](crate::RwStore).

use std::fmt::{Debug, Display, Formatter};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::{fmt, mem};

use crate::block::Block;
use crate::id::Id;

/// A read lock for an element in an [RwStore](crate::RwStore).
///
/// The lock will automatically be released when this is dropped.
///
/// # Example
///
/// ```
/// # use read_write_store::RwStore;
/// # use read_write_store::timeout::Timeout::DontBlock;
/// # use std::mem;
/// let store = RwStore::new();
/// let id = store.insert(42);
///
/// let read_lock = store.read(id).unwrap();
/// assert_eq!(*read_lock, 42);
///
/// assert!(store.write_with_timeout(id, DontBlock).is_err());
/// mem::drop(read_lock);
/// assert!(store.write_with_timeout(id, DontBlock).is_ok());
/// ```
pub struct ReadLock<'a, Element> {
    id: Id,
    _marker: PhantomData<(&'a Element, *const ())>,
}

impl<'a, Element> ReadLock<'a, Element> {
    pub(super) unsafe fn new(id: Id) -> Self {
        Self {
            id,
            _marker: PhantomData::default(),
        }
    }
}

impl<'a, Element> Deref for ReadLock<'a, Element> {
    type Target = Element;

    fn deref(&self) -> &Element {
        unsafe { Block::get_unchecked(self.id).as_ref() }
    }
}

impl<'a, Element> Drop for ReadLock<'a, Element> {
    fn drop(&mut self) {
        unsafe {
            Block::<Element>::unlock_read(self.id);
        }
    }
}

impl<'a, Element: Debug> Debug for ReadLock<'a, Element> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Debug::fmt(self.deref(), f)
    }
}

impl<'a, Element: Display> Display for ReadLock<'a, Element> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Display::fmt(self.deref(), f)
    }
}

unsafe impl<'a, Element: Sync> Sync for ReadLock<'a, Element> {}

/// A write lock for an element in an [RwStore](crate::RwStore).
///
/// The lock will automatically be released when this is dropped.
///
/// # Example
///
/// ```
/// # use read_write_store::RwStore;
/// # use read_write_store::timeout::Timeout::DontBlock;
/// # use std::mem;
/// let store = RwStore::new();
/// let id = store.insert(42);
///
/// let mut write_lock = store.write(id).unwrap();
/// *write_lock = 24;
/// assert_eq!(*write_lock, 24);
///
/// assert!(store.read_with_timeout(id, DontBlock).is_err());
/// mem::drop(write_lock);
/// assert!(store.read_with_timeout(id, DontBlock).is_ok());
/// ```
pub struct WriteLock<'a, Element> {
    id: Id,
    _marker: PhantomData<(&'a Element, *const ())>,
}

impl<'a, Element> WriteLock<'a, Element> {
    pub(super) unsafe fn new(id: Id) -> Self {
        Self {
            id,
            _marker: PhantomData::default(),
        }
    }

    pub(super) fn forget(self) -> Id {
        let result = self.id;
        mem::forget(self);
        result
    }
}

impl<'a, Element> Deref for WriteLock<'a, Element> {
    type Target = Element;

    fn deref(&self) -> &Element {
        unsafe { Block::get_unchecked(self.id).as_ref() }
    }
}

impl<'a, Element> DerefMut for WriteLock<'a, Element> {
    fn deref_mut(&mut self) -> &mut Element {
        unsafe { Block::get_unchecked(self.id).as_mut() }
    }
}

impl<'a, Element> Drop for WriteLock<'a, Element> {
    fn drop(&mut self) {
        unsafe { Block::<Element>::unlock_write(self.id) }
    }
}

impl<'a, Element: Debug> Debug for WriteLock<'a, Element> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Debug::fmt(self.deref(), f)
    }
}

impl<'a, Element: Display> Display for WriteLock<'a, Element> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Display::fmt(self.deref(), f)
    }
}

unsafe impl<'a, Element: Sync> Sync for WriteLock<'a, Element> {}

#[cfg(test)]
mod test {
    use crate::RwStore;
    use std::panic::{RefUnwindSafe, UnwindSafe};

    #[test]
    fn read_lock_implements_sync() {
        let store = RwStore::new();
        let id = store.insert(42);
        let lock = store.read(id).unwrap();
        &lock as &dyn Sync;
    }

    #[test]
    fn write_lock_implements_sync() {
        let store = RwStore::new();
        let id = store.insert(42);
        let lock = store.write(id).unwrap();
        &lock as &dyn Sync;
    }

    #[test]
    fn read_lock_implements_unwind_safe() {
        let store = RwStore::new();
        let id = store.insert(42);
        let lock = store.read(id).unwrap();
        &lock as &dyn UnwindSafe;
    }

    #[test]
    fn write_lock_implements_unwind_safe() {
        let store = RwStore::new();
        let id = store.insert(42);
        let lock = store.write(id).unwrap();
        &lock as &dyn UnwindSafe;
    }

    #[test]
    fn read_lock_implements_ref_unwind_safe() {
        let store = RwStore::new();
        let id = store.insert(42);
        let lock = store.read(id).unwrap();
        &lock as &dyn RefUnwindSafe;
    }

    #[test]
    fn write_lock_implements_ref_unwind_safe() {
        let store = RwStore::new();
        let id = store.insert(42);
        let lock = store.write(id).unwrap();
        &lock as &dyn RefUnwindSafe;
    }
}

mod doctest {
    /// ```compile_fail
    /// use read_write_store::RwStore;
    /// let store = RwStore::new();
    /// let id = store.insert(42);
    /// let lock = store.read(id).unwrap();
    /// &lock as &dyn Send;
    /// ```
    #[test]
    fn read_lock_doesnt_implement_send() {}

    /// ```compile_fail
    /// use read_write_store::RwStore;
    /// let store = RwStore::new();
    /// let id = store.insert(42);
    /// let lock = store.write(id).unwrap();
    /// &lock as &dyn Send;
    /// ```
    #[test]
    fn write_lock_doesnt_implement_send() {}
}
