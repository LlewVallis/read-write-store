#[cfg(debug_assertions)]
use std::thread;
use std::time::Instant;

use crate::timeout::{BlockResult, TimedOut};
use crate::util::sync::atomic::{AtomicU64, Ordering};
use crate::util::sync::park::{Park, ParkChoice, ParkResult};
use crate::Timeout;

pub const RESERVED_ID: u32 = u32::MAX;

/// The maximum number of read locks which can be held concurrently.
///
/// Acquiring more than this number of read locks simultaneously will panic.
pub const MAX_CONCURRENT_READS: u32 = (1 << 31) - 2;

/// An unsafe read-write lock with ID matching. As well as the lock state, a header stores an ID for
/// the data it is guarding. Many operations on the header take an ID which is compared to the
/// stored ID to determine whether to proceed with the operation.
pub struct Header {
    // The layout of the header's state is as follows:
    // * The most significant bit is the thread notification bit. This is set when no threads are
    //   blocking on the header and is unset when there are threads blocking. The behavior of this
    //   bit is the opposite what you might expect because the most significant 32 bits being set
    //   represents a special state.
    // * The next 31 most significant bits store the number of readers or is all ones if the header
    //   is write locked. If the header is unlocked, these bits will be zero.
    // * There are never any threads blocking on an unlocked header, so it is invalid for the thread
    //   notification bit to be unset (indicating one or more waiting threads) and for the reader
    //   bits to be all unset. This bit pattern instead represents the header being in an unoccupied
    //   state.
    // * The lower 32 bits store the ID if the header is occupied, or store the next ID if the
    //   header is unoccupied.
    state: Park<AtomicU64>,
}

impl Header {
    pub fn new() -> Self {
        let state = Self::unoccupied_bits(0);

        debug_assert!(state == 0, "initial state was not zeroed");

        Self {
            state: Park::new(AtomicU64::new(state)),
        }
    }

    /// Locks the header for reading if it's ID matches and it is not write locked. If the IDs match
    /// the header must be occupied.
    pub unsafe fn lock_read(&self, id: u32, timeout: Timeout) -> BlockResult<bool> {
        debug_assert!(id != RESERVED_ID, "attempted to read lock the reserved ID");

        let current = self.state.load(Ordering::Relaxed);
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
            let current = self.state.load(Ordering::Relaxed);

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
        let current = self.state.load(Ordering::Relaxed);
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

    /// Write locks the header if its ID matches and it is not locked. If the IDs match the header
    /// must be occupied.
    pub unsafe fn lock_write(&self, id: u32, timeout: Timeout) -> BlockResult<bool> {
        debug_assert!(id != RESERVED_ID, "attempted to write lock the reserved ID");

        self.transition(
            Self::occupied_unlocked_bits(id),
            Self::write_locked_bits(id),
            timeout,
        )
    }

    /// Write unlocks the header. The header must be currently write locked and the ID must match
    /// this header's current ID.
    pub unsafe fn unlock_write(&self, id: u32) {
        let new = Self::occupied_unlocked_bits(id);
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

    /// Moves the header from an unoccupied state into an occupied one, returning the ID of the
    /// newly occupied header. The header must be in an unoccupied state.
    pub unsafe fn occupy(&self) -> u32 {
        let old = self
            .state
            .fetch_or(Self::thread_notification_mask(), Ordering::AcqRel);

        debug_assert!(
            !Self::is_occupied(old),
            "attempted to occupy occupied header"
        );

        debug_assert!(
            Self::id_from_bits(old) != RESERVED_ID,
            "attempted to occupy header with the reserved ID"
        );

        Self::id_from_bits(old)
    }

    /// Increments the ID of the header and moves it into the unoccupied state if the ID matches. If
    /// the IDs match, the header must be occupied.
    pub unsafe fn remove(&self, id: u32, timeout: Timeout) -> BlockResult<RemoveResult> {
        debug_assert!(id != RESERVED_ID, "attempted to remove the reserved ID");

        let next_id = id + 1;

        let matched = self.transition(
            Self::occupied_unlocked_bits(id),
            Self::unoccupied_bits(next_id),
            timeout,
        )?;

        if matched {
            Ok(RemoveResult::Matched {
                may_reuse: next_id != RESERVED_ID,
            })
        } else {
            Ok(RemoveResult::DidntMatch)
        }
    }

    /// Write unlocks the header, increments the ID and moves it into the unoccupied state. The
    /// header's ID must match and it must be in the write locked state. Returns whether the header
    /// can be reused.
    pub unsafe fn remove_locked(&self, id: u32) -> bool {
        let next_id = id + 1;

        let new = Self::unoccupied_bits(next_id);
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

        next_id != RESERVED_ID
    }

    /// Sets the state of the header to the new state if it is currently in the expected state. If
    /// the expected value did not match, the thread will block until it does. If the ID of the
    /// actual state is different to the ID of the expected state, this will fail.
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
    pub fn needs_drop(&mut self) -> bool {
        Self::is_occupied(self.state.load_directly())
    }

    /// Determines the ID the header is currently tracking.
    pub fn id(&mut self) -> u32 {
        let state = self.state.load_directly();
        Self::id_from_bits(state)
    }

    pub fn id_if_occupied(&mut self) -> Option<u32> {
        let state = self.state.load_directly();

        if Self::is_occupied(state) {
            Some(Self::id_from_bits(state))
        } else {
            None
        }
    }

    /// Puts the header into the unoccupied state, returning the header's ID if it was occupied.
    pub fn reset(&mut self) -> Option<u32> {
        let state = self.state.load_directly();

        debug_assert!(
            Self::readers_from_bits(state) == 0,
            "header had readers (0x{:x}) when being reset",
            Self::readers_from_bits(state),
        );

        if Self::is_occupied(state) {
            let id = Self::id_from_bits(state);

            debug_assert!(
                !Self::has_thread_blocking(state),
                "header had thread blocking when being reset"
            );

            self.state.store_directly(Self::unoccupied_bits(id));

            Some(id)
        } else {
            None
        }
    }

    fn compare_exchange(&self, expected: u64, new: u64) -> Result<u64, u64> {
        self.state
            .compare_exchange(expected, new, Ordering::Release, Ordering::Relaxed)
    }

    fn compare_exchange_weak(&self, expected: u64, new: u64) -> Result<u64, u64> {
        self.state
            .compare_exchange_weak(expected, new, Ordering::Release, Ordering::Relaxed)
    }

    fn unoccupied_bits(id: u32) -> u64 {
        id as u64
    }

    fn occupied_unlocked_bits(id: u32) -> u64 {
        Self::thread_notification_mask() | Self::unoccupied_bits(id)
    }

    fn thread_notification_mask() -> u64 {
        1u64 << 63
    }

    fn is_occupied(state: u64) -> bool {
        state >> 32 != 0
    }

    fn write_locked_bits(id: u32) -> u64 {
        (id as u64) | ((u32::MAX as u64) << 32)
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
        debug_assert!(
            Self::is_occupied(bits),
            "cannot check thread blocking status when unoccupied"
        );

        bits & Self::thread_notification_mask() == 0
    }

    fn mark_thread_blocking(bits: u64) -> u64 {
        debug_assert!(
            Self::readers_from_bits(bits) > 0,
            "cannot block when unlocked"
        );

        debug_assert!(
            Self::id_from_bits(bits) != RESERVED_ID,
            "cannot block on the reserved ID"
        );

        bits & !Self::thread_notification_mask()
    }

    fn unmark_thread_blocking(bits: u64) -> u64 {
        bits | Self::thread_notification_mask()
    }

    fn increment_readers(bits: u64) -> u64 {
        if Self::readers_from_bits(bits) == MAX_CONCURRENT_READS {
            Self::too_many_readers();
        }

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

    #[inline(never)]
    fn too_many_readers() -> ! {
        panic!("too many concurrent readers on RwStore element")
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
                !Self::is_occupied(state) || !Self::has_thread_blocking(state),
                "header had thread blocking when being dropped"
            );
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum RemoveResult {
    Matched { may_reuse: bool },
    DidntMatch,
}

enum BlockChoice<T> {
    Block(u64),
    DontBlock(T),
}

#[cfg(test)]
mod test {
    use crate::header::{Header, RemoveResult};
    use crate::timeout::TimedOut;
    use crate::timeout::Timeout::DontBlock;

    #[test]
    fn reset_initially_returns_none() {
        let mut header = Header::new();
        assert_eq!(header.reset(), None);
    }

    #[test]
    fn reset_returns_the_tracked_id() {
        unsafe {
            let mut header = Header::new();
            let id = header.occupy();

            assert_eq!(header.reset(), Some(id));
        }

        unsafe {
            let mut header = Header::new();
            let id = header.occupy();
            header.remove(id, DontBlock).unwrap();
            let id = header.occupy();

            assert_eq!(header.reset(), Some(id));
        }
    }

    #[test]
    fn reset_returns_none_after_double_invocation() {
        unsafe {
            let mut header = Header::new();
            header.occupy();

            header.reset();
            assert_eq!(header.reset(), None);
        }
    }

    #[test]
    fn needs_drop_is_false_initially() {
        let mut header = Header::new();
        assert!(!header.needs_drop());
    }

    #[test]
    fn needs_drop_is_true_after_occupation() {
        unsafe {
            let mut header = Header::new();
            header.occupy();

            assert!(header.needs_drop());
        }
    }

    #[test]
    fn needs_drop_is_false_after_removal() {
        unsafe {
            let mut header = Header::new();
            let id = header.occupy();

            header.remove(id, DontBlock).unwrap();
            assert!(!header.needs_drop());
        }
    }

    #[test]
    fn needs_drop_is_false_after_locked_removal() {
        unsafe {
            let mut header = Header::new();
            let id = header.occupy();

            header.lock_write(id, DontBlock).unwrap();
            header.remove_locked(id);

            assert!(!header.needs_drop());
        }
    }

    #[test]
    fn lock_read_succeeds_when_id_matches() {
        unsafe {
            let header = Header::new();
            let id = header.occupy();

            assert_eq!(header.lock_read(id, DontBlock), Ok(true));
            header.unlock_read(id);
        }
    }

    #[test]
    fn lock_write_succeeds_when_id_matches() {
        unsafe {
            let header = Header::new();
            let id = header.occupy();

            assert_eq!(header.lock_write(id, DontBlock), Ok(true));
            header.unlock_write(id);
        }
    }

    #[test]
    fn lock_read_fails_when_id_doesnt_match() {
        unsafe {
            let header = Header::new();
            let id = header.occupy();

            assert_eq!(header.lock_read(id + 1, DontBlock), Ok(false));
        }
    }

    #[test]
    fn lock_write_fails_when_id_doesnt_match() {
        unsafe {
            let header = Header::new();
            let id = header.occupy();

            assert_eq!(header.lock_write(id + 1, DontBlock), Ok(false));
        }
    }

    #[test]
    fn double_read_lock_succeeds() {
        unsafe {
            let header = Header::new();
            let id = header.occupy();

            header.lock_read(id, DontBlock).unwrap();
            assert_eq!(header.lock_read(id, DontBlock), Ok(true));
            header.unlock_read(id);
            header.unlock_read(id);
        }
    }

    #[test]
    fn remove_succeeds_when_id_matches() {
        unsafe {
            let header = Header::new();
            let id = header.occupy();

            assert_eq!(
                header.remove(id, DontBlock),
                Ok(RemoveResult::Matched { may_reuse: true })
            );
        }
    }

    #[test]
    fn remove_fails_when_id_doesnt_match() {
        unsafe {
            let header = Header::new();
            let id = header.occupy();

            assert_eq!(
                header.remove(id + 1, DontBlock),
                Ok(RemoveResult::DidntMatch)
            );
        }
    }

    #[test]
    fn remove_fails_before_occupation() {
        unsafe {
            let header = Header::new();
            assert_eq!(header.remove(42, DontBlock), Ok(RemoveResult::DidntMatch));
        }
    }

    #[test]
    fn remove_fails_after_double_invocation() {
        unsafe {
            let header = Header::new();
            let id = header.occupy();

            header.remove(id, DontBlock).unwrap();
            assert_eq!(header.remove(id, DontBlock), Ok(RemoveResult::DidntMatch));
        }
    }

    #[test]
    fn cannot_lock_read_when_locking_write() {
        unsafe {
            let header = Header::new();
            let id = header.occupy();

            header.lock_write(id, DontBlock).unwrap();
            assert_eq!(header.lock_read(id, DontBlock), Err(TimedOut));
            header.unlock_write(id);
        }
    }

    #[test]
    fn cannot_lock_write_when_locking_read() {
        unsafe {
            let header = Header::new();
            let id = header.occupy();

            header.lock_read(id, DontBlock).unwrap();
            assert_eq!(header.lock_write(id, DontBlock), Err(TimedOut));
            header.unlock_read(id);
        }
    }

    #[test]
    fn cannot_remove_when_locking_read() {
        unsafe {
            let header = Header::new();
            let id = header.occupy();

            header.lock_read(id, DontBlock).unwrap();
            assert_eq!(header.remove(id, DontBlock), Err(TimedOut));
            header.unlock_read(id);
        }
    }

    #[test]
    fn cannot_remove_when_locking_write() {
        unsafe {
            let header = Header::new();
            let id = header.occupy();

            header.lock_write(id, DontBlock).unwrap();
            assert_eq!(header.remove(id, DontBlock), Err(TimedOut));
            header.unlock_write(id);
        }
    }
}
