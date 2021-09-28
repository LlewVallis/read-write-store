macro_rules! create_atomic {
    ($name:ident, $type:ty, $native:path) => {
        pub struct $name {
            inner: $native,
        }

        impl $name {
            #[allow(unused)]
            pub fn new(value: $type) -> Self {
                type Native = $native;
                Self {
                    inner: Native::new(value),
                }
            }

            #[cfg(loom)]
            #[allow(unused)]
            pub fn store_directly(&mut self, value: $type) {
                self.store(value, Ordering::SeqCst)
            }

            #[cfg(loom)]
            #[allow(unused)]
            pub fn load_directly(&mut self) -> $type {
                self.load(Ordering::SeqCst)
            }

            #[cfg(not(loom))]
            #[allow(unused)]
            pub fn store_directly(&mut self, value: $type) {
                *self.inner.get_mut() = value;
            }

            #[cfg(not(loom))]
            #[allow(unused)]
            pub fn load_directly(&mut self) -> $type {
                *self.inner.get_mut()
            }

            #[allow(unused)]
            pub fn store(&self, value: $type, ordering: Ordering) {
                self.inner.store(value, ordering.native())
            }

            #[allow(unused)]
            pub fn load(&self, ordering: Ordering) -> $type {
                self.inner.load(ordering.native())
            }

            #[allow(unused)]
            pub fn swap(&self, new: $type, ordering: Ordering) -> $type {
                self.inner.swap(new, ordering.native())
            }

            #[allow(unused)]
            pub fn compare_exchange(
                &self,
                expected: $type,
                new: $type,
                success: Ordering,
                failure: Ordering,
            ) -> Result<$type, $type> {
                self.inner
                    .compare_exchange(expected, new, success.native(), failure.native())
            }

            #[allow(unused)]
            pub fn compare_exchange_weak(
                &self,
                expected: $type,
                new: $type,
                success: Ordering,
                failure: Ordering,
            ) -> Result<$type, $type> {
                self.inner
                    .compare_exchange_weak(expected, new, success.native(), failure.native())
            }

            #[allow(unused)]
            pub fn fetch_add(&self, value: $type, ordering: Ordering) -> $type {
                self.inner.fetch_add(value, ordering.native())
            }

            #[allow(unused)]
            pub fn fetch_sub(&self, value: $type, ordering: Ordering) -> $type {
                self.inner.fetch_sub(value, ordering.native())
            }

            #[allow(unused)]
            pub fn fetch_or(&self, value: $type, ordering: Ordering) -> $type {
                self.inner.fetch_or(value, ordering.native())
            }

            #[allow(unused)]
            pub fn into_inner(mut self) -> $type {
                self.load_directly()
            }
        }
    };
}

#[cfg(loom)]
create_atomic!(AtomicU32, u32, loom::sync::atomic::AtomicU32);
#[cfg(loom)]
create_atomic!(AtomicU64, u64, loom::sync::atomic::AtomicU64);
#[cfg(loom)]
create_atomic!(AtomicUsize, usize, loom::sync::atomic::AtomicUsize);

#[cfg(not(loom))]
create_atomic!(AtomicU32, u32, std::sync::atomic::AtomicU32);
#[cfg(not(loom))]
create_atomic!(AtomicU64, u64, std::sync::atomic::AtomicU64);
#[cfg(not(loom))]
create_atomic!(AtomicUsize, usize, std::sync::atomic::AtomicUsize);
#[allow(unused)]
#[derive(Copy, Clone)]
pub enum Ordering {
    Relaxed,
    Acquire,
    Release,
    AcqRel,
    SeqCst,
}

#[cfg(loom)]
impl Ordering {
    fn native(self) -> loom::sync::atomic::Ordering {
        match self {
            Ordering::Relaxed => loom::sync::atomic::Ordering::Relaxed,
            Ordering::Acquire => loom::sync::atomic::Ordering::Acquire,
            Ordering::Release => loom::sync::atomic::Ordering::Release,
            Ordering::AcqRel => loom::sync::atomic::Ordering::AcqRel,
            Ordering::SeqCst => loom::sync::atomic::Ordering::SeqCst,
        }
    }
}

#[cfg(not(loom))]
impl Ordering {
    fn native(self) -> std::sync::atomic::Ordering {
        match self {
            Ordering::Relaxed => std::sync::atomic::Ordering::Relaxed,
            Ordering::Acquire => std::sync::atomic::Ordering::Acquire,
            Ordering::Release => std::sync::atomic::Ordering::Release,
            Ordering::AcqRel => std::sync::atomic::Ordering::AcqRel,
            Ordering::SeqCst => std::sync::atomic::Ordering::SeqCst,
        }
    }
}
