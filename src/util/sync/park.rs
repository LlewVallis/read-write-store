use std::ops::{Deref, DerefMut};
use std::time::Instant;

#[cfg(loom)]
pub struct Park<Inner> {
    inner: Inner,
    lock: loom::sync::Mutex<()>,
    condition: loom::sync::Condvar,
}

#[cfg(loom)]
impl<Inner> Park<Inner> {
    pub fn new(value: Inner) -> Self {
        Self {
            inner: value,
            lock: loom::sync::Mutex::new(()),
            condition: loom::sync::Condvar::new(),
        }
    }

    pub unsafe fn park<T>(
        this: &Self,
        timeout: Option<Instant>,
        f: impl FnOnce() -> ParkChoice<T>,
    ) -> ParkResult<T> {
        debug_assert!(
            timeout.is_none(),
            "cannot park with timeout when using loom"
        );

        let guard = this.lock.lock().unwrap();

        match f() {
            ParkChoice::Park => {
                this.condition.wait(guard).unwrap();
                ParkResult::Waited
            }
            ParkChoice::DontPark(result) => ParkResult::DidntPark(result),
        }
    }

    pub unsafe fn unpark(this: &Self) {
        let _guard = this.lock.lock().unwrap();
        this.condition.notify_all();
    }
}

#[cfg(not(loom))]
pub struct Park<Inner> {
    inner: Inner,
}

#[cfg(not(loom))]
impl<Inner> Park<Inner> {
    pub fn new(value: Inner) -> Self {
        Self { inner: value }
    }

    pub unsafe fn park<T>(
        this: &Self,
        timeout: Option<Instant>,
        f: impl FnOnce() -> ParkChoice<T>,
    ) -> ParkResult<T> {
        use crate::util::debug_check::DebugCheckedMaybeUninit;
        use std::cell::UnsafeCell;
        use std::hint::unreachable_unchecked;

        let addr = this as *const _ as usize;
        let result = UnsafeCell::new(DebugCheckedMaybeUninit::uninit());

        parking_lot::park(
            addr,
            || match f() {
                ParkChoice::Park => {
                    *result.get() = DebugCheckedMaybeUninit::new(ParkResult::Waited);
                    true
                }
                ParkChoice::DontPark(value) => {
                    *result.get() = DebugCheckedMaybeUninit::new(ParkResult::DidntPark(value));
                    false
                }
            },
            || {},
            |_, _| match result.get().read().assume_init() {
                ParkResult::Waited => {
                    *result.get() = DebugCheckedMaybeUninit::new(ParkResult::TimedOut);
                }
                _ => {
                    if cfg!(debug_assertions) {
                        unreachable!()
                    } else {
                        unreachable_unchecked()
                    }
                }
            },
            timeout,
        );

        result.get().read().assume_init()
    }

    pub unsafe fn unpark(this: &Self) {
        let addr = this as *const _ as usize;
        parking_lot::unpark_all(addr);
    }
}

impl<Inner> Deref for Park<Inner> {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.inner
    }
}

impl<Inner> DerefMut for Park<Inner> {
    fn deref_mut(&mut self) -> &mut Inner {
        &mut self.inner
    }
}

pub enum ParkChoice<T> {
    Park,
    DontPark(T),
}

pub enum ParkResult<T> {
    Waited,
    #[allow(unused)]
    TimedOut,
    DidntPark(T),
}
