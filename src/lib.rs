//! Key-aware update channel with cooldown coalescing and minimal diff delivery.
//!
//! A `keywatch` channel helps keep a conceptual key -> value store on a receiver in sync with a
//! sender by emitting only the minimal sequence of change events required for convergence. Updates
//! are per-key and can be either `Add(value)` (insert / upsert) or `Delete` (removal). The channel
//! aggressively coalesces redundant intermediate updates while still preserving enough ordering to
//! let the receiver reconstruct the latest state.
//!
//! Core behaviors:
//! * Two kinds of Add coalescing:
//!   1. Pre-delivery coalescing: multiple Adds for the same key sent before the *first* one is
//!      received collapse so only the last value is ever seen (even with zero cooldown). This keeps
//!      the pending set minimal when the receiver is idle or busy elsewhere.
//!   2. Cooldown coalescing: after an Add is delivered, further Adds within the `cooldown` window
//!      are withheld (only the most recent retained) and then emitted exactly once when the window
//!      elapses.
//! * Deletes are never delayed by cooldown; they preempt any pending Add for their key.
//! * Redundant Deletes are deduplicated: at most one Delete is emitted per present key until an Add
//!   recreates it; Deletes for unknown keys are suppressed.
//! * Clear emits a single Delete for each key currently known to the receiver (and cancels any
//!   pending Add values in transit).
//! * Ordering: Updates are yielded in FIFO order of their *effective* send time. Regular updates
//!   take their actual `send` call time; a cooled Add occupies a slot whose effective time is the
//!   cooldown expiry (later replacement Adds for that key before expiry only mutate the value in
//!   that slot). Thus matured cooled Adds appear in the order their cooldown slots were first
//!   created, interleaved with other keys' updates whose effective times arrive earlier.
//!
//! Example distinguishing the two coalescing modes:
//! ```rust
//! # #[tokio::main(flavor = "current_thread")] async fn main() {
//! use std::time::Duration;
//! use keywatch::{channel, Update};
//!
//! // 1. Pre-delivery coalescing (zero cooldown): burst before first receive.
//! let (tx0, mut rx0) = channel::<&'static str, i32>(Duration::ZERO);
//! for v in 1..=5 { tx0.send("k", Update::Add(v)).unwrap(); }
//! // Only the last value (5) is observed immediately.
//! assert_eq!(rx0.recv().await, Some(("k", Update::Add(5))));
//!
//! // 2. Cooldown coalescing (non-zero cooldown): subsequent burst after an observed Add.
//! let cooldown = Duration::from_millis(80);
//! let (tx, mut rx) = channel::<&'static str, i32>(cooldown);
//! tx.send("k", Update::Add(10)).unwrap();
//! assert_eq!(rx.recv().await, Some(("k", Update::Add(10))));
//! for v in 11..=15 { tx.send("k", Update::Add(v)).unwrap(); }
//! // The second value is withheld until cooldown expiry; measure elapsed.
//! let t_start = std::time::Instant::now();
//! let second = rx.recv().await.unwrap();
//! let elapsed = t_start.elapsed();
//! assert_eq!(second, ("k", Update::Add(15)));
//! assert!(elapsed >= cooldown);
//! # }
//! ```
use std::{collections::HashSet, hash::Hash, sync::Arc, time::Duration};

use parking_lot::Mutex;
use tokio::{select, sync::Notify};

use crate::raw::Raw;

mod raw;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
/// A change notification for a key.
pub enum Update<V> {
    /// Insert / upsert value for the key.
    Add(V),
    /// Remove the key; future redundant Deletes are suppressed until an Add occurs.
    Delete,
}

impl<V> Update<V> {
    pub fn into_option(self) -> Option<V> {
        match self {
            Update::Add(v) => Some(v),
            Update::Delete => None,
        }
    }
}

/// Create a channel with an empty initial key set and specified Add cooldown duration.
pub fn channel<K, V>(cooldown: Duration) -> (Sender<K, V>, Receiver<K, V>) {
    channel_with_starting_keys(HashSet::new(), cooldown)
}

/// Create a channel specifying keys the receiver is assumed to already possess.
///
/// Deletes for these "starting" keys will be emitted even if no prior Add was seen in this
/// session.
pub fn channel_with_starting_keys<K, V>(
    starting_keys: HashSet<K>,
    cooldown: Duration,
) -> (Sender<K, V>, Receiver<K, V>) {
    let channel = Arc::new(Channel {
        inner: Mutex::new(Inner {
            raw: Raw::new(starting_keys, cooldown),
            dropped: false,
        }),
        changed: Notify::new(),
    });
    (Sender(Arc::clone(&channel)), Receiver(channel))
}

struct Channel<K, V> {
    inner: Mutex<Inner<K, V>>,
    changed: Notify,
}

struct Inner<K, V> {
    raw: Raw<K, V>,
    dropped: bool,
}

pub struct Sender<K, V>(Arc<Channel<K, V>>);

impl<K: Clone + Eq + Hash, V> Sender<K, V> {
    /// Send an Add or Delete for the key.
    ///
    /// Returns `Err(SendError(original_update))` if the channel is disconnected.
    pub fn send(&self, key: K, update: Update<V>) -> Result<(), SendError<V>> {
        let Channel { inner, changed } = self.0.as_ref();
        let replaced = {
            let mut inner = inner.lock();
            let Inner { raw, dropped } = &mut *inner;
            if *dropped {
                return Err(SendError(update));
            }
            raw.send(key, update)
        };
        changed.notify_waiters();
        drop(replaced);
        Ok(())
    }

    /// Emit Deletes for all keys currently known to the receiver, cancelling pending Adds.
    pub fn clear(&self) -> Result<(), SendError<V>> {
        let Channel { inner, changed } = self.0.as_ref();
        {
            let mut inner = inner.lock();
            let Inner { raw, dropped } = &mut *inner;
            if *dropped {
                return Err(SendError(Update::Delete));
            }
            raw.clear();
        }
        changed.notify_waiters();
        Ok(())
    }
}

#[derive(Debug)]
pub struct SendError<V>(pub Update<V>);

pub struct Receiver<K, V>(Arc<Channel<K, V>>);

impl<K: Clone + Eq + Hash, V> Receiver<K, V> {
    /// Await the next update; returns `None` when channel is fully disconnected and drained.
    pub async fn recv(&mut self) -> Option<(K, Update<V>)> {
        let Channel { inner, changed } = self.0.as_ref();
        loop {
            let notified_fut = changed.notified();
            let next_cooldown = {
                let mut inner = inner.lock();
                let Inner { raw, dropped } = &mut *inner;
                if let Some(received) = raw.try_recv() {
                    return Some(received);
                }
                if *dropped {
                    return None;
                }
                raw.wait_next_cooldown()
            };
            select! {
                () = notified_fut => {},
                () = next_cooldown => {},
            }
        }
    }

    /// Non-blocking receive attempt.
    pub fn try_recv(&mut self) -> Result<(K, Update<V>), TryRecvError> {
        let mut inner = self.0.inner.lock();
        let Inner { raw, dropped } = &mut *inner;
        match raw.try_recv() {
            Some(output) => Ok(output),
            None => Err(TryRecvError::from_dropped(*dropped)),
        }
    }

    /// Attempt to receive the next update, _ignoring_ cooldowns.
    pub fn try_recv_now(&mut self) -> Result<(K, Update<V>), TryRecvError> {
        let mut inner = self.0.inner.lock();
        let Inner { raw, dropped } = &mut *inner;
        match raw.try_recv_now() {
            Some(output) => Ok(output),
            None => Err(TryRecvError::from_dropped(*dropped)),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TryRecvError {
    Disconnected,
    Empty,
}

impl TryRecvError {
    fn from_dropped(dropped: bool) -> Self {
        if dropped {
            Self::Disconnected
        } else {
            Self::Empty
        }
    }
}

impl<K, V> Drop for Sender<K, V> {
    fn drop(&mut self) {
        let Channel { inner, changed } = self.0.as_ref();
        inner.lock().dropped = true;
        changed.notify_waiters();
    }
}

impl<K, V> Drop for Receiver<K, V> {
    fn drop(&mut self) {
        let Channel { inner, changed } = self.0.as_ref();
        inner.lock().dropped = true;
        changed.notify_waiters();
    }
}
