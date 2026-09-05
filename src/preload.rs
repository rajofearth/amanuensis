//! RAM-aware engine preload policy.
//!
//! Startup loads the record engine immediately only when the
//! `preload_engines` config flag is on, total RAM is at least 8 GiB, and at
//! least 2.5 GiB is currently free. Otherwise engines load per use (first F9)
//! through the existing pending_start + FinishOnboarding path.
//!
//! Idle eviction (5 min with no recording activity) and the hourly total-RAM
//! recheck are parameterized here; the UI wiring lives in `main.rs`.
//! Threshold math ([`decide`], [`should_evict`]) is pure so unit tests stay
//! hermetic — they assert only threshold math, never real RAM.

use std::time::{Duration, Instant};

use crate::log;

/// Minimum total RAM that allows preload (8 GiB).
pub const MIN_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Minimum currently available RAM that allows preload (2.5 GiB).
pub const MIN_FREE_BYTES: u64 = 2 * 1024 * 1024 * 1024 + 512 * 1024 * 1024;
/// Idle time with no recording activity before the worker is released.
pub const IDLE_EVICT_AFTER: Duration = Duration::from_secs(5 * 60);
/// Background total-RAM recheck interval.
pub const RAM_RECHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Point-in-time RAM readings in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RamSnapshot {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// Outcome of the preload decision plus a log-ready reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreloadVerdict {
    pub preload: bool,
    pub reason: &'static str,
}

/// Pure threshold math: no sysinfo, no I/O. All unit tests go through here.
pub fn decide(preload_flag: bool, total_bytes: u64, free_bytes: u64) -> PreloadVerdict {
    if !preload_flag {
        return PreloadVerdict {
            preload: false,
            reason: "preload disabled in config; engines load per use",
        };
    }
    if total_bytes < MIN_TOTAL_BYTES {
        return PreloadVerdict {
            preload: false,
            reason: "total RAM below 8 GiB; engines load per use",
        };
    }
    if free_bytes < MIN_FREE_BYTES {
        return PreloadVerdict {
            preload: false,
            reason: "free RAM below 2.5 GiB; engines load per use",
        };
    }
    PreloadVerdict {
        preload: true,
        reason: "preload on with total RAM >= 8 GiB and free RAM >= 2.5 GiB",
    }
}

/// Read total + available RAM via sysinfo (same source as telemetry).
pub fn read_ram() -> RamSnapshot {
    let sys = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_memory(sysinfo::MemoryRefreshKind::everything()),
    );
    RamSnapshot {
        total_bytes: sys.total_memory(),
        free_bytes: sys.available_memory(),
    }
}

/// Startup check: read RAM once, decide, and log the reason.
pub fn should_preload(preload_flag: bool) -> (PreloadVerdict, RamSnapshot) {
    let snapshot = read_ram();
    let verdict = decide(preload_flag, snapshot.total_bytes, snapshot.free_bytes);
    log!(
        "preload",
        "decision preload={} ({}; total={:.1} GiB free={:.1} GiB flag={preload_flag})",
        verdict.preload,
        verdict.reason,
        snapshot.total_bytes as f64 / 1_073_741_824.0,
        snapshot.free_bytes as f64 / 1_073_741_824.0
    );
    (verdict, snapshot)
}

/// Pure idle math: evict once `now - last_activity` reaches the timeout.
pub fn should_evict(last_activity: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_activity) >= IDLE_EVICT_AFTER
}

/// Hourly total-RAM recheck on its own thread. Log-only: it never touches the
/// worker or the UI, so it can never block a frame; a flipped verdict is
/// logged prominently and the next startup (or reload) picks it up.
pub fn spawn_ram_watch(preload_flag: bool, initial: PreloadVerdict) {
    let spawn = std::thread::Builder::new()
        .name("preload-ram-watch".to_owned())
        .spawn(move || {
            let mut previous = initial;
            loop {
                std::thread::sleep(RAM_RECHECK_INTERVAL);
                let snapshot = read_ram();
                let next = decide(preload_flag, snapshot.total_bytes, snapshot.free_bytes);
                if next.preload != previous.preload {
                    log!(
                        "preload",
                        "RAM verdict flipped to preload={} ({}; total={:.1} GiB free={:.1} GiB)",
                        next.preload,
                        next.reason,
                        snapshot.total_bytes as f64 / 1_073_741_824.0,
                        snapshot.free_bytes as f64 / 1_073_741_824.0
                    );
                    previous = next;
                } else {
                    log!(
                        "preload",
                        "RAM recheck: preload={} unchanged ({})",
                        next.preload,
                        next.reason
                    );
                }
            }
        });
    if let Err(error) = spawn {
        log!("preload", "RAM watch thread failed to start: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn allows_preload_at_exact_thresholds() {
        let verdict = decide(true, MIN_TOTAL_BYTES, MIN_FREE_BYTES);
        assert!(verdict.preload);
        assert!(!verdict.reason.is_empty());
    }

    #[test]
    fn denies_preload_one_byte_under_total() {
        let verdict = decide(true, MIN_TOTAL_BYTES - 1, 16 * GIB);
        assert!(!verdict.preload);
    }

    #[test]
    fn denies_preload_one_byte_under_free() {
        let verdict = decide(true, 16 * GIB, MIN_FREE_BYTES - 1);
        assert!(!verdict.preload);
    }

    #[test]
    fn flag_off_denies_despite_ample_ram() {
        let verdict = decide(false, 64 * GIB, 32 * GIB);
        assert!(!verdict.preload);
    }

    #[test]
    fn evicts_only_after_five_minutes_idle() {
        let last = Instant::now();
        assert!(!should_evict(
            last,
            last + IDLE_EVICT_AFTER - Duration::from_secs(1)
        ));
        assert!(should_evict(last, last + IDLE_EVICT_AFTER));
    }

    #[test]
    fn evict_never_fires_on_reordered_clock() {
        let earlier = Instant::now();
        let later = earlier + Duration::from_secs(10);
        assert!(!should_evict(later, earlier));
    }
}
