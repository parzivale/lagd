//! A bounded, time-stamped queue that releases items once they have aged.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Longest a drainer blocks before re-reading the delay from shared memory.
///
/// The delay lives in a file written by another process, so there is nothing to
/// notify on when it changes. Capping each wait bounds how long a *shrinking*
/// delay takes to take effect; 5 ms costs ~200 idle wakeups a second, which is
/// nothing next to the input rate of a gaming mouse.
pub const RECHECK: Duration = Duration::from_millis(5);

/// A delay line of `T`, stamped on push and released on age.
///
/// One producer thread pushes, one consumer thread drains. Items are released
/// strictly in order, so nothing can overtake anything else no matter how the
/// delay moves while they are queued.
pub struct DelayLine<T> {
    inner: Mutex<Inner<T>>,
    wake: Condvar,
    capacity: usize,
}

struct Inner<T> {
    items: VecDeque<(Instant, T)>,
    closed: bool,
    dropped: u64,
}

impl<T> DelayLine<T> {
    /// # Panics
    ///
    /// If `capacity` is zero, which would make every push a drop.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "a delay line needs room for at least one item"
        );
        Self {
            inner: Mutex::new(Inner {
                items: VecDeque::with_capacity(capacity),
                closed: false,
                dropped: 0,
            }),
            wake: Condvar::new(),
            capacity,
        }
    }

    /// A poisoned delay line still has to drain: the alternative is a grabbed
    /// keyboard that has gone silent, which is far worse than acting on
    /// possibly-stale state.
    fn lock(&self) -> MutexGuard<'_, Inner<T>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stamps `item` with the current instant and queues it.
    ///
    /// At capacity the *oldest* item is discarded: the queue only fills when
    /// the consumer has fallen behind, and in that state the freshest input is
    /// the input worth keeping.
    pub fn push(&self, item: T) {
        let mut g = self.lock();
        if g.items.len() == self.capacity {
            g.items.pop_front();
            g.dropped += 1;
        }
        g.items.push_back((Instant::now(), item));
        drop(g);
        self.wake.notify_one();
    }

    /// Blocks until the head has aged enough, then returns it.
    ///
    /// `delay` is re-evaluated on every wakeup, so a live change to the control
    /// plane is picked up within [`RECHECK`] without the caller doing anything.
    /// `None` from the closure means the stage is bypassed, which drains the
    /// line as fast as it can rather than sleeping — the caller is on its way
    /// out of the signal path and must not sit on queued events.
    ///
    /// Returns `None` only once the line is closed *and* empty.
    pub fn pop_due<F>(&self, delay: F) -> Option<T>
    where
        F: Fn() -> Option<Duration>,
    {
        let mut g = self.lock();
        loop {
            match g.items.front() {
                Some((stamp, _)) => {
                    let wait = match delay() {
                        None => Duration::ZERO,
                        Some(d) => d.saturating_sub(stamp.elapsed()),
                    };
                    if wait.is_zero() {
                        return g.items.pop_front().map(|(_, item)| item);
                    }
                    g = self.wait(g, wait.min(RECHECK));
                }
                None if g.closed => return None,
                None => g = self.wait(g, RECHECK),
            }
        }
    }

    /// Pops the head regardless of age. Used to flush on shutdown.
    pub fn pop_now(&self) -> Option<T> {
        self.lock().items.pop_front().map(|(_, item)| item)
    }

    /// How long the oldest queued item has been waiting, if any.
    ///
    /// The watchdog compares this against the configured delay: a value far
    /// past it means the consumer has stalled, which on the input stage means
    /// a live-but-silent keyboard and has to trigger a fail-open.
    #[must_use]
    pub fn oldest_age(&self) -> Option<Duration> {
        self.lock().items.front().map(|(stamp, _)| stamp.elapsed())
    }

    /// Items discarded because the consumer could not keep up.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stops accepting work and lets a drained `pop_due` return `None`.
    pub fn close(&self) {
        self.lock().closed = true;
        self.wake.notify_all();
    }

    fn wait<'a>(&self, g: MutexGuard<'a, Inner<T>>, for_: Duration) -> MutexGuard<'a, Inner<T>> {
        self.wake
            .wait_timeout(g, for_)
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn fixed(ms: u64) -> impl Fn() -> Option<Duration> {
        move || Some(Duration::from_millis(ms))
    }

    #[test]
    fn an_item_is_held_for_the_delay() {
        let line = DelayLine::new(8);
        line.push(1);
        let start = Instant::now();
        assert_eq!(line.pop_due(fixed(40)), Some(1));
        let held = start.elapsed();
        assert!(
            held >= Duration::from_millis(38),
            "released after only {held:?}"
        );
    }

    #[test]
    fn bypass_drains_without_waiting() {
        let line = DelayLine::new(8);
        line.push(7);
        let start = Instant::now();
        assert_eq!(line.pop_due(|| None), Some(7));
        assert!(start.elapsed() < Duration::from_millis(5));
    }

    /// The case that motivates re-reading the delay inside the wait loop: an
    /// item parked behind a long delay has to come out early when the delay is
    /// dropped mid-flight, not serve out its original sentence.
    #[test]
    fn shrinking_the_delay_releases_a_parked_item() {
        let line = Arc::new(DelayLine::new(8));
        let long = Arc::new(std::sync::atomic::AtomicU64::new(400));
        line.push(3);

        let start = Instant::now();
        let reader = {
            let (line, long) = (Arc::clone(&line), Arc::clone(&long));
            thread::spawn(move || {
                line.pop_due(|| {
                    Some(Duration::from_millis(
                        long.load(std::sync::atomic::Ordering::Relaxed),
                    ))
                })
            })
        };

        thread::sleep(Duration::from_millis(30));
        long.store(0, std::sync::atomic::Ordering::Relaxed);

        assert_eq!(reader.join().unwrap(), Some(3));
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "parked item served its original delay instead of the new one"
        );
    }

    #[test]
    fn order_is_preserved_across_a_delay_change() {
        let line = DelayLine::new(8);
        for i in 0..4 {
            line.push(i);
        }
        line.close();
        let mut out = Vec::new();
        while let Some(i) = line.pop_due(|| Some(Duration::ZERO)) {
            out.push(i);
        }
        assert_eq!(out, vec![0, 1, 2, 3]);
    }

    #[test]
    fn overflow_drops_the_stalest_input() {
        let line = DelayLine::new(2);
        line.push('a');
        line.push('b');
        line.push('c');
        line.close();
        assert_eq!(line.dropped(), 1);
        assert_eq!(line.pop_due(|| None), Some('b'));
        assert_eq!(line.pop_due(|| None), Some('c'));
        assert_eq!(line.pop_due(|| None), None);
    }
}
