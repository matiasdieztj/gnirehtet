//! Best-effort correlation between ADB serials and relay TCP connections.
//!
//! The relay accepts TCP connections from `adb reverse` tunnels on a single
//! port. The ADB daemon does not forward the device serial along with the
//! connection, so the relay would otherwise only see `127.0.0.1:<port>`.
//!
//! To recover the serial, callers register it right before triggering the
//! connection (`cmd_start` does this just before running `adb reverse` and
//! `am start`). When the relay accepts a new connection, it consumes the
//! oldest pending entry.
//!
//! This is a timing-based heuristic: it assumes the device connects shortly
//! after the reverse tunnel is set up. In practice the round-trip is well
//! under a second, so a wide match window (30 s) makes the correlation
//! effectively reliable for typical use.
//!
//! The registry is global state shared between the binary crate (which
//! registers serials) and the library crate (which consumes them). Since
//! the binary links against the library, both see the same static.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a registered serial stays eligible for matching.
const MATCH_WINDOW: Duration = Duration::from_secs(30);

/// Upper bound on the pending queue. Protects against pathological cases
/// (many registrations without matching connections).
const MAX_PENDING: usize = 8;

struct PendingEntry {
    serial: String,
    registered_at: Instant,
}

fn queue() -> &'static Mutex<VecDeque<PendingEntry>> {
    static QUEUE: OnceLock<Mutex<VecDeque<PendingEntry>>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Register `serial` as "about to open a connection to the relay".
///
/// Called by `cmd_start` before running `adb reverse` and starting the
/// client on the device.
pub fn register_pending(serial: &str) {
    let mut q = queue().lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    // Drop entries that fell out of the match window.
    q.retain(|e| now.duration_since(e.registered_at) < MATCH_WINDOW);
    // Enforce the upper bound: drop the oldest if full.
    if q.len() >= MAX_PENDING {
        q.pop_front();
    }
    q.push_back(PendingEntry {
        serial: serial.to_string(),
        registered_at: now,
    });
}

/// Take the oldest pending serial, if any and still within the match
/// window. Called by the relay when a new TCP connection is accepted.
pub fn take_pending() -> Option<String> {
    let mut q = queue().lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    // Drop expired entries from the front (FIFO order).
    while let Some(front) = q.front() {
        if now.duration_since(front.registered_at) >= MATCH_WINDOW {
            q.pop_front();
        } else {
            break;
        }
    }
    q.pop_front().map(|e| e.serial)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The registry is global state, so tests must run serially and clear
    // the queue between runs. Use a mutex to serialize access.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset() {
        let mut q = queue().lock().unwrap_or_else(|e| e.into_inner());
        q.clear();
    }

    #[test]
    fn register_then_take() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        register_pending("serial-A");
        assert_eq!(take_pending(), Some("serial-A".to_string()));
        assert_eq!(take_pending(), None);
    }

    #[test]
    fn fifo_order() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        register_pending("A");
        register_pending("B");
        register_pending("C");
        assert_eq!(take_pending(), Some("A".to_string()));
        assert_eq!(take_pending(), Some("B".to_string()));
        assert_eq!(take_pending(), Some("C".to_string()));
        assert_eq!(take_pending(), None);
    }

    #[test]
    fn take_empty_returns_none() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        assert_eq!(take_pending(), None);
    }

    #[test]
    fn max_pending_drops_oldest() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        for i in 0..(MAX_PENDING + 3) {
            register_pending(&format!("s{}", i));
        }
        // The oldest three entries should have been dropped.
        assert_eq!(take_pending(), Some("s3".to_string()));
    }
}
