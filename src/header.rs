#[cfg(debug_assertions)]
use std::thread;
use std::time::Instant;

use crate::timeout::{BlockResult, TimedOut};
use crate::util::sync::atomic::{AtomicU64, Ordering};
use crate::util::sync::park::{Park, ParkChoice, ParkResult};
use crate::{Timeout, RESERVED_ID};

/// An unsafe read-write lock with ID matching. As well as the lock state, a header stores an ID for
/// the data it is guarding. Many operations on the header take an ID which is compared to the
/// stored ID to determine whether to proceed with the operation.
pub struct Header {
    // The lower 32 bits store the ID, the next highest 31 bits store either the number of readers,
    // or all ones if there is a writer, the most significant bit is set when a thread needs to be
    // notified. When there are no readers, the thread notification bit can not be set. There are
    // never any readers when the header's ID is zeroed. The thread notification bit is always set
    // before a thread starts to park and is always unset before all threads are unparked
    state: Park<AtomicU64>,
}

impl Header {
    pub fn new() -> Self {
        Self {
            state: Park::new(AtomicU64::new(Self::unlocked_bits(RESERVED_ID))),
        }
    }

    /// Locks the header for reading if it's ID matches and it is not write locked.
    pub unsafe fn lock_read(&self, id: u32, timeout: Timeout) -> BlockResult<bool> {
        debug_assert!(id != RESERVED_ID, "attempted to read lock the reserved ID");

        let current = self.state.load(Ordering::Acquire);
        self.lock_read_with_current(id, current, timeout)
    }

    unsafe fn lock_read_with_current(
        &self,
        id: u32,
        current: u64,
        timeout: Timeout,
    ) -> BlockResult<bool> {
        if Self::id_from_bits(current) != id {
            return Ok(false);
        }

        if Self::is_write_locked(current) {
            return self.lock_read_slow(id, timeout);
        }

        match self.compare_exchange_weak(current, Self::increment_readers(current)) {
            Ok(_) => Ok(true),
            Err(actual) => self.lock_read_with_current(id, actual, timeout),
        }
    }

    #[inline(never)]
    unsafe fn lock_read_slow(&self, id: u32, timeout: Timeout) -> BlockResult<bool> {
        enum Response {
            Matched(u64),
            Mismatch,
        }

        let timeout_optional = match timeout {
            Timeout::DontBlock => return BlockResult::Err(TimedOut),
            Timeout::BlockIndefinitely => None,
            Timeout::BlockUntil(deadline) => Some(deadline),
        };

        let result = self.block(timeout_optional, || {
            let current = self.state.load(Ordering::Acquire);

            if Self::id_from_bits(current) != id {
                return BlockChoice::DontBlock(Response::Mismatch);
            }

            if Self::is_write_locked(current) {
                return BlockChoice::Block(current);
            }

            BlockChoice::DontBlock(Response::Matched(current))
        });

        match result {
            Ok(Response::Matched(current)) => {
                match self.compare_exchange_weak(current, Self::increment_readers(current)) {
                    Ok(_) => Ok(true),
                    Err(actual) => self.lock_read_with_current(id, actual, timeout),
                }
            }
            Ok(Response::Mismatch) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Read unlocks the header. The header must be currently read locked and the ID must match the
    /// header's current ID.
    pub unsafe fn unlock_read(&self, id: u32) {
        let current = self.state.load(Ordering::Acquire);
        self.unlock_read_with_current(id, current)
    }

    unsafe fn unlock_read_with_current(&self, id: u32, current: u64) {
        debug_assert!(
            Self::readers_from_bits(current) != 0,
            "attempted to read unlock already unlocked header"
        );

        debug_assert!(
            !Self::is_write_locked(current),
            "attempted to read unlock write locked header"
        );

        debug_assert!(
            Self::id_from_bits(current) == id,
            "attempted to read unlock with ID 0x{:x} but it was actually 0x{:x}",
            id,
            Self::id_from_bits(current)
        );

        let must_unpark =
            Self::has_thread_blocking(current) && Self::readers_from_bits(current) == 1;

        let new = if must_unpark {
            Self::unmark_thread_blocking(Self::decrement_readers(current))
        } else {
            Self::decrement_readers(current)
        };

        match self.compare_exchange_weak(current, new) {
            Ok(_) => {
                if must_unpark {
                    Park::unpark(&self.state)
                }
            }
            Err(actual) => self.unlock_read_with_current(id, actual),
        }
    }

    /// Write locks the header if its ID matches and it is not locked.
    pub unsafe fn lock_write(&self, id: u32, timeout: Timeout) -> BlockResult<bool> {
        debug_assert!(id != RESERVED_ID, "attempted to read lock the reserved ID");
        self.transition(
            Self::unlocked_bits(id),
            Self::write_locked_bits(id),
            timeout,
        )
    }

    /// Write unlocks the header. The header must be currently write locked and the ID must match
    /// this header's current ID.
    pub unsafe fn unlock_write(&self, id: u32) {
        let new = Self::unlocked_bits(id);
        let old = self.state.swap(new, Ordering::AcqRel);

        debug_assert!(
            Self::id_from_bits(old) == id,
            "attempted to write unlock with ID 0x{:x} but it was actually 0x{:x}",
            id,
            Self::id_from_bits(old)
        );

        debug_assert!(
            Self::is_write_locked(old),
            "attempted to write unlock header that was not write locked"
        );

        if Self::has_thread_blocking(old) {
            Park::unpark(&self.state)
        }
    }

    /// Sets the ID of the header and leaves it in an unlocked state. The header must be in the
    /// zeroed state. It is not safe to race invocations to insert.
    pub unsafe fn insert(&self, id: u32) {
        debug_assert!(id != RESERVED_ID, "attempted to insert the reserved ID");

        let new = Self::unlocked_bits(id);

        if cfg!(debug_assertions) {
            let old = self.state.swap(new, Ordering::AcqRel);
            debug_assert!(
                old == Self::unlocked_bits(RESERVED_ID),
                "attempted to insert into header in use"
            );
        } else {
            self.state.store(new, Ordering::Release);
        }
    }

    /// Zero's the ID of the header and leaves it ready for insertion if the ID matches and the
    /// header is not locked.
    pub unsafe fn remove(&self, id: u32, timeout: Timeout) -> BlockResult<bool> {
        debug_assert!(id != RESERVED_ID, "attempted to remove the reserved ID");
        self.transition(
            Self::unlocked_bits(id),
            Self::unlocked_bits(RESERVED_ID),
            timeout,
        )
    }

    /// Zero's the ID of the header and leaves it ready for insertion. The header must be write
    /// locked and the ID must match the header's current ID.
    pub unsafe fn remove_locked(&self, id: u32) {
        let new = Self::unlocked_bits(RESERVED_ID);
        let old = self.state.swap(new, Ordering::AcqRel);

        debug_assert!(
            Self::id_from_bits(old) == id,
            "attempted to write unlock with ID 0x{:x} but it was actually 0x{:x}",
            id,
            Self::id_from_bits(old)
        );

        debug_assert!(
            Self::is_write_locked(old),
            "attempted to write unlock header that was not write locked"
        );

        if Self::has_thread_blocking(old) {
            Park::unpark(&self.state)
        }
    }

    /// Sets the state of the header to the new state if it is currently in the expected state. If
    /// the expected value did not match, the thread will block until it does.
    unsafe fn transition(&self, expected: u64, new: u64, timeout: Timeout) -> BlockResult<bool> {
        match self.compare_exchange_weak(expected, new) {
            Ok(_) => Ok(true),
            Err(actual) => {
                if Self::id_from_bits(actual) == Self::id_from_bits(expected) {
                    if Self::readers_from_bits(actual) > 0 {
                        self.transition_slow(expected, new, timeout)
                    } else {
                        self.transition(expected, new, timeout)
                    }
                } else {
                    Ok(false)
                }
            }
        }
    }

    #[inline(never)]
    unsafe fn transition_slow(
        &self,
        expected: u64,
        new: u64,
        timeout: Timeout,
    ) -> BlockResult<bool> {
        let timeout = match timeout {
            Timeout::DontBlock => return BlockResult::Err(TimedOut),
            Timeout::BlockIndefinitely => None,
            Timeout::BlockUntil(deadline) => Some(deadline),
        };

        self.block(timeout, move || {
            match self.compare_exchange(expected, new) {
                Ok(_) => BlockChoice::DontBlock(true),
                Err(actual) => {
                    if Self::id_from_bits(actual) == Self::id_from_bits(expected) {
                        BlockChoice::Block(actual)
                    } else {
                        BlockChoice::DontBlock(false)
                    }
                }
            }
        })
    }

    /// Performs an operation atomically with respect to unparking which may either return a final
    /// result or decide to block. If the operation decides to block, it must return an expected
    /// value for the current state of the header. This will be used to set the thread notification
    /// bit of the header with a CAS operation. If the CAS fails, the operation will be run again
    /// until it succeeds. If blocking is successful, upon wakeup the entire process will be run
    /// again until the operation decides not to block.
    unsafe fn block<T, F>(&self, timeout: Option<Instant>, f: F) -> BlockResult<T>
    where
        F: Fn() -> BlockChoice<T>,
    {
        match Park::park(&self.state, timeout, || {
            self.block_result_to_park_result(&f)
        }) {
            ParkResult::Waited => self.block(timeout, f),
            ParkResult::TimedOut => Err(TimedOut),
            ParkResult::DidntPark(result) => Ok(result),
        }
    }

    fn block_result_to_park_result<T, F>(&self, f: &F) -> ParkChoice<T>
    where
        F: Fn() -> BlockChoice<T>,
    {
        match f() {
            BlockChoice::Block(expected_state) => {
                let new_state = Self::mark_thread_blocking(expected_state);

                if self.compare_exchange(expected_state, new_state).is_ok() {
                    ParkChoice::Park
                } else {
                    self.block_result_to_park_result(f)
                }
            }
            BlockChoice::DontBlock(result) => ParkChoice::DontPark(result),
        }
    }

    /// Determines whether or not the header is tracking an element.
    pub fn is_occupied(&mut self) -> bool {
        self.state.load_directly() != 0
    }

    /// Determines the ID the header is currently tracking.
    pub fn id(&mut self) -> u32 {
        let state = self.state.load_directly();
        Self::id_from_bits(state)
    }

    /// Zeroes the header, returning the ID it was tracking if any. The header must not be locked.
    pub fn reset(&mut self) -> Option<u32> {
        debug_assert!(
            Self::readers_from_bits(self.state.load_directly()) == 0,
            "header had readers (0x{:x}) when being reset",
            Self::readers_from_bits(self.state.load_directly()),
        );

        debug_assert!(
            !Self::has_thread_blocking(self.state.load_directly()),
            "header had thread blocking when being reset"
        );

        let id = Self::id_from_bits(self.state.load_directly());
        self.state.store_directly(Self::unlocked_bits(RESERVED_ID));

        if id == RESERVED_ID {
            None
        } else {
            Some(id)
        }
    }

    fn compare_exchange(&self, expected: u64, new: u64) -> Result<u64, u64> {
        self.state
            .compare_exchange(expected, new, Ordering::AcqRel, Ordering::Acquire)
    }

    fn compare_exchange_weak(&self, expected: u64, new: u64) -> Result<u64, u64> {
        self.state
            .compare_exchange_weak(expected, new, Ordering::AcqRel, Ordering::Acquire)
    }

    fn unlocked_bits(id: u32) -> u64 {
        id as u64
    }

    fn write_locked_bits(id: u32) -> u64 {
        id as u64 | ((u32::MAX as u64) << 32)
    }

    fn id_from_bits(bits: u64) -> u32 {
        bits as u32
    }

    fn readers_from_bits(bits: u64) -> u32 {
        (bits >> 32) as u32 & !(1u32 << 31)
    }

    fn is_write_locked(bits: u64) -> bool {
        Self::readers_from_bits(bits) == !(1u32 << 31)
    }

    fn has_thread_blocking(bits: u64) -> bool {
        bits & (1u64 << 63) != 0
    }

    fn mark_thread_blocking(bits: u64) -> u64 {
        debug_assert!(
            Self::readers_from_bits(bits) > 0,
            "cannot block when unlocked"
        );

        debug_assert!(
            Self::id_from_bits(bits) != RESERVED_ID,
            "cannot block when empty"
        );

        bits | (1u64 << 63)
    }

    fn unmark_thread_blocking(bits: u64) -> u64 {
        bits & !(1u64 << 63)
    }

    fn increment_readers(bits: u64) -> u64 {
        debug_assert!(
            !Self::is_write_locked(bits),
            "cannot add reader when write locked"
        );

        debug_assert!(
            Self::id_from_bits(bits) != RESERVED_ID,
            "cannot lock when empty"
        );

        bits + (1 << 32)
    }

    fn decrement_readers(bits: u64) -> u64 {
        debug_assert!(Self::readers_from_bits(bits) != 0, "no readers to remove");
        bits - (1 << 32)
    }
}

#[cfg(debug_assertions)]
impl Drop for Header {
    fn drop(&mut self) {
        if !thread::panicking() {
            let state = self.state.load_directly();

            debug_assert!(
                Self::readers_from_bits(state) == 0,
                "header had readers (0x{:x}) when being dropped",
                Self::readers_from_bits(state),
            );

            debug_assert!(
                !Self::has_thread_blocking(state),
                "header had thread blocking when being dropped"
            );
        }
    }
}

enum BlockChoice<T> {
    Block(u64),
    DontBlock(T),
}

#[cfg(test)]
mod test {
    use crate::header::Header;
    use crate::timeout::TimedOut;
    use crate::timeout::Timeout::DontBlock;

    #[test]
    fn reset_initially_returns_none() {
        let mut header = Header::new();
        assert_eq!(header.reset(), None);
    }

    #[test]
    fn reset_returns_the_inserted_id() {
        unsafe {
            let mut header = Header::new();
            header.insert(42);

            assert_eq!(header.reset(), Some(42));
        }
    }

    #[test]
    fn reset_returns_none_after_double_invocation() {
        unsafe {
            let mut header = Header::new();
            header.insert(42);

            header.reset();
            assert_eq!(header.reset(), None);
        }
    }

    #[test]
    fn is_occupied_is_false_initially() {
        let mut header = Header::new();
        assert!(!header.is_occupied());
    }

    #[test]
    fn is_occupied_is_true_after_insertion() {
        unsafe {
            let mut header = Header::new();
            header.insert(42);

            assert!(header.is_occupied());
        }
    }

    #[test]
    fn is_occupied_is_false_after_removal() {
        unsafe {
            let mut header = Header::new();
            header.insert(42);

            header.remove(42, DontBlock).unwrap();
            assert!(!header.is_occupied());
        }
    }

    #[test]
    fn is_occupied_is_false_after_locked_removal() {
        unsafe {
            let mut header = Header::new();
            header.insert(42);

            header.lock_write(42, DontBlock).unwrap();
            header.remove_locked(42);

            assert!(!header.is_occupied());
        }
    }

    #[test]
    fn lock_read_fails_before_insertion() {
        unsafe {
            let header = Header::new();
            assert_eq!(header.lock_read(42, DontBlock), Ok(false));
        }
    }

    #[test]
    fn lock_write_fails_before_insertion() {
        unsafe {
            let header = Header::new();
            assert_eq!(header.lock_write(42, DontBlock), Ok(false));
        }
    }

    #[test]
    fn lock_read_succeeds_when_id_matches() {
        unsafe {
            let header = Header::new();
            header.insert(42);

            assert_eq!(header.lock_read(42, DontBlock), Ok(true));
            header.unlock_read(42);
        }
    }

    #[test]
    fn lock_write_succeeds_when_id_matches() {
        unsafe {
            let header = Header::new();
            header.insert(42);

            assert_eq!(header.lock_write(42, DontBlock), Ok(true));
            header.unlock_write(42);
        }
    }

    #[test]
    fn lock_read_fails_when_id_doesnt_match() {
        unsafe {
            let header = Header::new();
            header.insert(42);

            assert_eq!(header.lock_read(24, DontBlock), Ok(false));
        }
    }

    #[test]
    fn lock_write_fails_when_id_doesnt_match() {
        unsafe {
            let header = Header::new();
            header.insert(42);

            assert_eq!(header.lock_write(24, DontBlock), Ok(false));
        }
    }

    #[test]
    fn double_read_lock_succeeds() {
        unsafe {
            let header = Header::new();
            header.insert(42);

            header.lock_read(42, DontBlock).unwrap();
            assert_eq!(header.lock_read(42, DontBlock), Ok(true));
            header.unlock_read(42);
            header.unlock_read(42);
        }
    }

    #[test]
    fn remove_succeeds_when_id_matches() {
        unsafe {
            let header = Header::new();
            header.insert(42);

            assert_eq!(header.remove(42, DontBlock), Ok(true));
        }
    }

    #[test]
    fn remove_fails_when_id_doesnt_match() {
        unsafe {
            let header = Header::new();
            header.insert(42);

            assert_eq!(header.remove(24, DontBlock), Ok(false));
        }
    }

    #[test]
    fn remove_fails_before_insertion() {
        unsafe {
            let header = Header::new();
            assert_eq!(header.remove(42, DontBlock), Ok(false));
        }
    }

    #[test]
    fn remove_fails_after_double_invocation() {
        unsafe {
            let header = Header::new();
            header.insert(42);

            header.remove(42, DontBlock).unwrap();
            assert_eq!(header.remove(42, DontBlock), Ok(false));
        }
    }

    #[test]
    fn cannot_lock_read_when_locking_write() {
        unsafe {
            let header = Header::new();
            header.insert(42);

            header.lock_write(42, DontBlock).unwrap();
            assert_eq!(header.lock_read(42, DontBlock), Err(TimedOut));
            header.unlock_write(42);
        }
    }

    #[test]
    fn cannot_lock_write_when_locking_read() {
        unsafe {
            let header = Header::new();
            header.insert(42);

            header.lock_read(42, DontBlock).unwrap();
            assert_eq!(header.lock_write(42, DontBlock), Err(TimedOut));
            header.unlock_read(42);
        }
    }

    #[test]
    fn cannot_remove_when_locking_read() {
        unsafe {
            let header = Header::new();
            header.insert(42);

            header.lock_read(42, DontBlock).unwrap();
            assert_eq!(header.remove(42, DontBlock), Err(TimedOut));
            header.unlock_read(42);
        }
    }

    #[test]
    fn cannot_remove_when_locking_write() {
        unsafe {
            let header = Header::new();
            header.insert(42);

            header.lock_write(42, DontBlock).unwrap();
            assert_eq!(header.remove(42, DontBlock), Err(TimedOut));
            header.unlock_write(42);
        }
    }
}
