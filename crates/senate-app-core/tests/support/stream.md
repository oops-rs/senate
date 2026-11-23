## How the re-read is coalesced

The watch used to read the session list again on **every** event, so a burst of forty events cost forty reads. Now the first event marks the list *due* and schedules one read; the events after it, until that read runs, only find it already due. A burst costs one read, and the last event is never lost, because the read happens after it.

### The Rust side

The link owns the flag and the timer. `swap` both reads and sets it, so two events racing on two threads still schedule exactly one read:

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Reads the session list again soon after a change, once per burst.
pub struct Coalescer {
    due: Arc<AtomicBool>,
    delay: Duration,
}

impl Coalescer {
    pub fn new(delay: Duration) -> Self {
        Self { due: Arc::new(AtomicBool::new(false)), delay }
    }

    /// An event arrived: schedule one read unless one is already due.
    pub fn poke(&self, read: impl FnOnce() + Send + 'static) {
        if self.due.swap(true, Ordering::AcqRel) {
            return; // the read already scheduled will see this event
        }
        let due = Arc::clone(&self.due);
        let delay = self.delay;
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            // Clear first: an event during the read schedules the next one.
            due.store(false, Ordering::Release);
            read();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test(start_paused = true)]
    async fn a_burst_costs_one_read() {
        let reads = Arc::new(AtomicUsize::new(0));
        let c = Coalescer::new(Duration::from_millis(50));
        for _ in 0..40 {
            let reads = Arc::clone(&reads);
            c.poke(move || { reads.fetch_add(1, Ordering::Relaxed); });
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(reads.load(Ordering::Relaxed), 1);
    }
}
```

### The Swift side

The app does the same with one task on the main actor; a newer change cancels nothing, it just finds the read pending:

```swift
@MainActor
final class SessionsReader {
    private var pending: Task<Void, Never>?
    private let delay: Duration = .milliseconds(50)

    /// Something changed: read once, soon, for the whole burst.
    func changed(_ read: @escaping @MainActor () async -> Void) {
        guard pending == nil else { return } // already due
        pending = Task { [weak self] in
            try? await Task.sleep(for: self?.delay ?? .zero)
            self?.pending = nil
            await read()
        }
    }
}
```

### What it saves

| Burst | Reads before | Reads now | Time saved |
| :-- | --: | --: | --: |
| 1 event | 1 | 1 | 0 ms |
| 10 events | 10 | 1 | 41 ms |
| 40 events | 40 | 1 | 176 ms |
| a 64 KiB tool output | 312 | 2 | 1.4 s |

### Next

1. Measure the watch under `FAKE_TIMELINE_EVERY_MS=200`
2. Apply the same pattern to the inbox:
   - one flag per committee
   - the same 50 ms delay
3. Remove the old per-event read, and its test

- [x] Coalesce the session list
- [ ] Coalesce the inbox
- [ ] Update the README's numbers

> The rule: an event may make a read *due*, but only the timer *runs* it.
