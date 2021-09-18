//! Timeout specifiers and error types.
//!
//! See [Timeout](Timeout).

use std::time::{Duration, Instant};

/// A timeout (or lack thereof) describing how blocking operations should behave.
#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub enum Timeout {
    /// Disallows blocking for a contended resource.
    DontBlock,
    /// Allows blocking for a contended resource until the given time has passed.
    BlockUntil(Instant),
    /// Allows blocking for a contended resource indefinitely.
    BlockIndefinitely,
}

impl Timeout {
    /// Creates a new timeout that lasts for the provided duration, starting now.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::timeout::Timeout;
    /// # use std::time::{Instant, Duration};
    /// let first_timeout = Timeout::BlockUntil(Instant::now());
    /// let second_timeout = Timeout::block_for(Duration::from_secs(1));
    /// assert!(first_timeout < second_timeout);
    /// ```
    pub fn block_for(duration: Duration) -> Self {
        Self::BlockUntil(Instant::now() + duration)
    }

    /// Creates a new timeout that lasts for the provided number of milliseconds, starting now.
    ///
    /// # Example
    ///
    /// ```
    /// # use read_write_store::timeout::Timeout;
    /// # use std::time::Instant;
    /// let first_timeout = Timeout::BlockUntil(Instant::now());
    /// let second_timeout = Timeout::block_for_millis(1000);
    /// assert!(first_timeout < second_timeout);
    /// ```
    pub fn block_for_millis(millis: u64) -> Self {
        Self::block_for(Duration::from_millis(millis))
    }
}

/// A result which either succeeded, or failed due to a timeout.
pub type BlockResult<T> = Result<T, TimedOut>;

/// The value of the error alternative of [BlockResult](crate::BlockResult) that indicates a
/// timeout.
#[derive(Debug, Clone, Copy, Ord, PartialOrd, Hash, Eq, PartialEq)]
pub struct TimedOut;

#[cfg(test)]
mod test {
    use std::time::{Duration, Instant};

    use crate::Timeout;

    #[test]
    fn ordering_is_correct() {
        let now = Instant::now();

        let in_the_future = Timeout::BlockUntil(now + Duration::from_secs(1));
        let in_the_past = Timeout::BlockUntil(now - Duration::from_secs(1));

        let mut timeouts = vec![
            Timeout::BlockIndefinitely,
            in_the_future,
            in_the_past,
            Timeout::DontBlock,
        ];

        timeouts.sort();

        assert_eq!(
            timeouts,
            vec![
                Timeout::DontBlock,
                in_the_past,
                in_the_future,
                Timeout::BlockIndefinitely,
            ]
        );
    }
}
