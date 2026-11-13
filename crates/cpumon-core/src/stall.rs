//! Responsiveness probe: how late a normal-priority thread wakes up.
//!
//! A thread sleeps in short ticks and records how much later than requested it woke.
//! When the machine stalls (CPU starvation, a driver hogging the CPUs in DPCs, heavy
//! paging), every normal-priority thread is delayed, including this one. The probe's
//! short period catches freezes far shorter than the sampling interval, which a
//! once-per-sample check would usually miss.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often the probe wakes.
pub const PROBE_PERIOD: Duration = Duration::from_millis(250);

/// Lateness above this is assumed to be sleep/hibernate, not a stall.
const SUSPEND: Duration = Duration::from_secs(30);

/// A background thread measuring wake-up lateness. Stops when dropped.
pub struct StallProbe {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

struct Shared {
    stop: AtomicBool,
    /// Worst lateness since the last `take_max`, in microseconds.
    max_late_us: AtomicU64,
}

impl StallProbe {
    /// Starts the probe thread.
    pub fn start() -> std::io::Result<Self> {
        Self::with_period(PROBE_PERIOD)
    }

    fn with_period(period: Duration) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            max_late_us: AtomicU64::new(0),
        });
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("stall-probe".into())
            .stack_size(64 * 1024)
            .spawn(move || probe(&thread_shared, period))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// The worst wake-up lateness since the previous call, then resets it.
    pub fn take_max(&self) -> Duration {
        Duration::from_micros(self.shared.max_late_us.swap(0, Ordering::Relaxed))
    }
}

impl Drop for StallProbe {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

fn probe(shared: &Shared, period: Duration) {
    let mut due = Instant::now() + period;
    while !shared.stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if due > now {
            std::thread::park_timeout(due - now);
            continue;
        }
        let late = now - due;
        if late < SUSPEND {
            let us = u64::try_from(late.as_micros()).unwrap_or(u64::MAX);
            shared.max_late_us.fetch_max(us, Ordering::Relaxed);
        }
        due = now + period;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_small_lateness_on_an_idle_machine() {
        let probe = StallProbe::with_period(Duration::from_millis(10)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let late = probe.take_max();
        // Timer granularity alone can add ~16 ms; anything near a second means the
        // probe is not measuring what it should.
        assert!(late < Duration::from_millis(500), "{late:?}");
        drop(probe);
    }

    #[test]
    fn take_max_returns_worst_and_resets() {
        // A long period, so the thread does not record anything during the test.
        let probe = StallProbe::with_period(Duration::from_secs(60)).unwrap();
        probe.shared.max_late_us.fetch_max(1_500, Ordering::Relaxed);
        probe.shared.max_late_us.fetch_max(700, Ordering::Relaxed);
        assert_eq!(probe.take_max(), Duration::from_micros(1_500));
        assert_eq!(probe.take_max(), Duration::ZERO);
    }

    #[test]
    fn stops_promptly_when_dropped() {
        let probe = StallProbe::with_period(Duration::from_secs(60)).unwrap();
        let started = Instant::now();
        drop(probe);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
