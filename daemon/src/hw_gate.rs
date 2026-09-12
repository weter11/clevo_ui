//! Process-lifetime gate for hardware accesses that fail permanently.
//!
//! Some firmware paths fail deterministically. On this XMG APEX the Clevo
//! `_DSM` function 0x04 (flexicharger / charge-control state) aborts with
//! `AE_AML_BUFFER_LIMIT` inside the DSDT, and every failed call makes the ACPI
//! interpreter dump ~7 lines into the kernel log. A poll loop that keeps
//! re-reading such a value turns one firmware defect into gigabytes of
//! `/var/log` (2026-09: 4 GB in a week, root filesystem full).
//!
//! Rule: a target that is known-broken must not be polled again. The first
//! failure disables that capability for the daemon's process lifetime and logs
//! exactly one warning (same level and wording style as the companion kernel
//! patch in tuxedo-drivers, which logs `... (cmd 0x04) - command disabled`).
//! There is deliberately no retry/backoff path: retrying a call that can never
//! succeed only settles into producing noise at a lower rate. Capabilities are
//! re-armed on resume-from-suspend ([`rearm_after_resume`]), where the firmware
//! state may genuinely have changed; a daemon restart is the manual recheck.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// Battery charge control state (tuxedo flexicharger, Clevo `_DSM` cmd 0x04).
pub const BATTERY_CHARGE_CONTROL: &str = "battery charge control (_DSM cmd 0x04)";

fn disabled() -> &'static Mutex<HashSet<&'static str>> {
    static DISABLED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    DISABLED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Is `capability` still allowed to be accessed?
pub fn enabled(capability: &'static str) -> bool {
    !disabled().lock().unwrap().contains(capability)
}

/// Disable `capability` for the rest of the daemon's lifetime.
///
/// Returns `true` only for the first failure, so callers can log the
/// transition exactly once.
pub fn disable(capability: &'static str) -> bool {
    disabled().lock().unwrap().insert(capability)
}

/// Record a permanent failure of `capability`: disable it and log once.
pub fn note_failure(capability: &'static str) {
    if disable(capability) {
        log::warn!(
            target: "hw.gate",
            "read failed ({}) - capability disabled for this daemon run",
            capability
        );
    }
}

/// Re-arm every gated capability. Called after resume-from-suspend: the
/// firmware may behave differently in the new power state, so the next access
/// is a fresh probe. Returns the capabilities that were disabled.
pub fn rearm_all() -> Vec<&'static str> {
    let mut disabled = disabled().lock().unwrap();
    disabled.drain().collect()
}

/// Re-arm after resume and log it (best effort, called from the logind hook).
pub fn rearm_after_resume() {
    let rearmed = rearm_all();
    if !rearmed.is_empty() {
        log::info!(
            target: "hw.gate",
            "resume detected - re-arming disabled capabilities: {:?}",
            rearmed
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Distinct capability names per test: the gate is process-global and tests
    // run in parallel.
    const CAP_A: &str = "test capability a";
    const CAP_B: &str = "test capability b";
    const CAP_C: &str = "test capability c";

    #[test]
    fn capability_is_enabled_until_first_failure() {
        assert!(enabled(CAP_A));
        assert!(disable(CAP_A), "first failure must report a transition");
        assert!(!enabled(CAP_A));
    }

    #[test]
    fn later_failures_are_not_transitions() {
        assert!(disable(CAP_B));
        assert!(!disable(CAP_B), "second failure must not log again");
        assert!(!enabled(CAP_B));
    }

    #[test]
    fn capabilities_are_independent() {
        disable(CAP_C);
        assert!(!enabled(CAP_C));
        // Unrelated capabilities are unaffected by CAP_C being disabled.
        assert!(enabled("unrelated capability"));
    }

    #[test]
    fn rearm_all_clears_disabled_capabilities() {
        disable(CAP_C);
        let rearmed = rearm_all();
        assert!(rearmed.contains(&CAP_C));
        assert!(enabled(CAP_C));
    }
}
