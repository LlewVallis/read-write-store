use std::fmt::Debug;
#[cfg(debug_assertions)]
use std::mem::ManuallyDrop;
#[cfg(not(debug_assertions))]
use std::mem::MaybeUninit;
use std::ptr::NonNull;

pub trait IndexDebugChecked<T> {
    unsafe fn get_debug_checked(&self, index: usize) -> &T;
    unsafe fn get_debug_checked_mut(&mut self, index: usize) -> &mut T;
}

impl<T> IndexDebugChecked<T> for [T] {
    unsafe fn get_debug_checked(&self, index: usize) -> &T {
        if cfg!(debug_assertions) {
            &self[index]
        } else {
            self.get_unchecked(index)
        }
    }

    unsafe fn get_debug_checked_mut(&mut self, index: usize) -> &mut T {
        if cfg!(debug_assertions) {
            &mut self[index]
        } else {
            self.get_unchecked_mut(index)
        }
    }
}

pub trait UnwrapDebugChecked<T> {
    unsafe fn unwrap_debug_checked(self) -> T;
}

impl<T> UnwrapDebugChecked<T> for Option<T> {
    unsafe fn unwrap_debug_checked(self) -> T {
        if cfg!(debug_assertions) {
            self.unwrap()
        } else {
            self.unwrap_unchecked()
        }
    }
}

impl<T, E: Debug> UnwrapDebugChecked<T> for Result<T, E> {
    unsafe fn unwrap_debug_checked(self) -> T {
        if cfg!(debug_assertions) {
            self.unwrap()
        } else {
            self.unwrap_unchecked()
        }
    }
}

pub trait NewDebugChecked<T> {
    unsafe fn new_debug_checked(value: T) -> Self;
}

impl<T> NewDebugChecked<*mut T> for NonNull<T> {
    unsafe fn new_debug_checked(value: *mut T) -> Self {
        if cfg!(debug_assertions) {
            NonNull::new(value).unwrap()
        } else {
            NonNull::new_unchecked(value)
        }
    }
}

#[repr(transparent)]
#[cfg(debug_assertions)]
pub struct DebugCheckedMaybeUninit<T> {
    inner: ManuallyDrop<Option<T>>,
}

#[cfg(debug_assertions)]
impl<T> DebugCheckedMaybeUninit<T> {
    pub fn uninit() -> Self {
        Self {
            inner: ManuallyDrop::new(None),
        }
    }

    pub fn new(value: T) -> Self {
        Self {
            inner: ManuallyDrop::new(Some(value)),
        }
    }

    pub unsafe fn assume_init(self) -> T {
        ManuallyDrop::into_inner(self.inner).unwrap()
    }

    pub unsafe fn contents_ptr(&self) -> NonNull<T> {
        match self.inner.as_ref() {
            Some(value) => NonNull::new_debug_checked(value as *const _ as *mut _),
            None => unreachable!(),
        }
    }
}

#[repr(transparent)]
#[cfg(not(debug_assertions))]
pub struct DebugCheckedMaybeUninit<T> {
    inner: MaybeUninit<T>,
}

#[cfg(not(debug_assertions))]
impl<T> DebugCheckedMaybeUninit<T> {
    pub fn uninit() -> Self {
        Self {
            inner: MaybeUninit::uninit(),
        }
    }

    pub fn new(value: T) -> Self {
        Self {
            inner: MaybeUninit::new(value),
        }
    }

    pub unsafe fn assume_init(self) -> T {
        self.inner.assume_init()
    }

    pub unsafe fn contents_ptr(&self) -> NonNull<T> {
        NonNull::new_debug_checked(self as *const _ as *mut _)
    }
}
