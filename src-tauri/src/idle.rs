//! Caches that are let go of when they haven't been used for a while, so a big one (grammars, an
//! unpacked file) doesn't stay in memory for the rest of the session.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A cached value and when it was last used.
pub(crate) type Slot<T> = Mutex<Option<(T, Instant)>>;

/// Empties `slot` once what's in it hasn't been used for `after`, then calls `then`. Call it when
/// the slot is filled from empty: one thread watches it, and ends when it has emptied it.
pub(crate) fn drop_when_idle<T: Send + 'static>(slot: &'static Slot<T>, after: Duration, then: fn()) {
    let spawned = std::thread::Builder::new().name("idle-cache".into()).spawn(move || loop {
        std::thread::sleep(after / 4);
        let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
        if s.as_ref().is_some_and(|(_, used)| used.elapsed() < after) {
            continue;
        }
        let old = s.take();
        drop(s);
        drop(old);
        then();
        return;
    });
    if let Err(e) = spawned {
        // The cache then stays until the app quits, as it did before.
        eprintln!("Can't start the idle-cache thread: {e}");
    }
}
