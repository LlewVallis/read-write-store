use std::ops::{Deref, DerefMut};

#[cfg(loom)]
pub struct RwLock<Inner> {
    inner: loom::sync::RwLock<Inner>,
}

#[cfg(loom)]
impl<Inner> RwLock<Inner> {
    pub fn new(value: Inner) -> Self {
        Self {
            inner: loom::sync::RwLock::new(value),
        }
    }

    pub fn read(&self) -> RwLockReadGuard<Inner> {
        RwLockReadGuard {
            inner: self.inner.read().unwrap(),
        }
    }

    pub fn write(&self) -> RwLockWriteGuard<Inner> {
        RwLockWriteGuard {
            inner: self.inner.write().unwrap(),
        }
    }

    pub fn into_inner(self) -> Inner {
        self.inner.into_inner().unwrap()
    }

    pub fn get_mut(&mut self) -> &mut Inner {
        let mut guard = self.write();
        unsafe { &mut *(guard.deref_mut() as *mut _) }
    }
}

#[cfg(not(loom))]
pub struct RwLock<Inner> {
    inner: parking_lot::RwLock<Inner>,
}

#[cfg(not(loom))]
impl<Inner> RwLock<Inner> {
    pub fn new(value: Inner) -> Self {
        Self {
            inner: parking_lot::RwLock::new(value),
        }
    }

    pub fn read(&self) -> RwLockReadGuard<Inner> {
        RwLockReadGuard {
            inner: self.inner.read(),
        }
    }

    pub fn write(&self) -> RwLockWriteGuard<Inner> {
        RwLockWriteGuard {
            inner: self.inner.write(),
        }
    }

    pub fn into_inner(self) -> Inner {
        self.inner.into_inner()
    }

    pub fn get_mut(&mut self) -> &mut Inner {
        self.inner.get_mut()
    }
}

#[cfg(loom)]
pub struct RwLockReadGuard<'a, Inner> {
    inner: loom::sync::RwLockReadGuard<'a, Inner>,
}

#[cfg(not(loom))]
pub struct RwLockReadGuard<'a, Inner> {
    inner: parking_lot::RwLockReadGuard<'a, Inner>,
}

impl<'a, Inner> Deref for RwLockReadGuard<'a, Inner> {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        self.inner.deref()
    }
}

#[cfg(loom)]
pub struct RwLockWriteGuard<'a, Inner> {
    inner: loom::sync::RwLockWriteGuard<'a, Inner>,
}

#[cfg(not(loom))]
pub struct RwLockWriteGuard<'a, Inner> {
    inner: parking_lot::RwLockWriteGuard<'a, Inner>,
}

impl<'a, Inner> Deref for RwLockWriteGuard<'a, Inner> {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        self.inner.deref()
    }
}

impl<'a, Inner> DerefMut for RwLockWriteGuard<'a, Inner> {
    fn deref_mut(&mut self) -> &mut Inner {
        self.inner.deref_mut()
    }
}
