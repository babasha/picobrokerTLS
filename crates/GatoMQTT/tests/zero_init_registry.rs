//! Verifies whether `SessionRegistry` constructed via `mem::zeroed()` (the
//! state produced by `.psram_bss` NOLOAD + runtime zero-fill) is
//! observationally equivalent to `SessionRegistry::new()`.
//!
//! Hypothesis being tested: `Option<SessionState>` uses niche-based
//! discriminant whose all-zero pattern decodes as `Some(<empty>)` rather
//! than `None`, breaking `iter().count()` / `is_full()` for registries
//! placed in NOLOAD memory.

use GatoMQTT::router::RetainedStore;
use GatoMQTT::session::registry::SessionRegistry;

// defmt sink stubs (defmt is `unstable-test`-clean only with a sink linked).
#[unsafe(no_mangle)]
fn _defmt_acquire() {}
#[unsafe(no_mangle)]
fn _defmt_release() {}
#[unsafe(no_mangle)]
fn _defmt_flush() {}
#[unsafe(no_mangle)]
fn _defmt_write(_bytes: &[u8]) {}
#[unsafe(no_mangle)]
fn _defmt_timestamp(_fmt: defmt::Formatter<'_>) {}
#[unsafe(no_mangle)]
fn _defmt_panic() -> ! {
    panic!("defmt panic")
}
#[unsafe(no_mangle)]
static __DEFMT_MARKER_TRACE_START: u8 = 0;
#[unsafe(no_mangle)]
static __DEFMT_MARKER_TRACE_END: u8 = 0;
#[unsafe(no_mangle)]
static __DEFMT_MARKER_DEBUG_START: u8 = 0;
#[unsafe(no_mangle)]
static __DEFMT_MARKER_DEBUG_END: u8 = 0;
#[unsafe(no_mangle)]
static __DEFMT_MARKER_INFO_START: u8 = 0;
#[unsafe(no_mangle)]
static __DEFMT_MARKER_INFO_END: u8 = 0;
#[unsafe(no_mangle)]
static __DEFMT_MARKER_WARN_START: u8 = 0;
#[unsafe(no_mangle)]
static __DEFMT_MARKER_WARN_END: u8 = 0;
#[unsafe(no_mangle)]
static __DEFMT_MARKER_ERROR_START: u8 = 0;
#[unsafe(no_mangle)]
static __DEFMT_MARKER_ERROR_END: u8 = 0;

#[test]
fn zeroed_registry_has_zero_entries() {
    // SAFETY: This is the test we want to do. If `Option<SessionState>`
    // uses a niche such that all-zero == `Some`, this `unsafe { zeroed() }`
    // yields a registry whose slots are interpreted as `Some(<garbage>)`
    // and the assertion fails — confirming the bug.
    let registry: SessionRegistry<1, 4, 2> = unsafe { core::mem::zeroed() };
    let count = registry.iter().count();
    assert_eq!(
        count, 0,
        "BUG: zero-init registry shows {count} entries (expected 0). \
         Option<SessionState> uses niche encoding where all-zero == Some, \
         so .psram_bss + NOLOAD breaks SessionRegistry."
    );
}

#[test]
fn zeroed_retained_store_has_zero_entries() {
    // RetainedStore wraps heapless::Vec, which is documented as
    // zero-init-safe (len=0, buffer MaybeUninit). Confirm.
    let store: RetainedStore<16> = unsafe { core::mem::zeroed() };
    assert_eq!(store.len(), 0);
    assert_eq!(store.iter().count(), 0);
}

#[test]
fn fresh_registry_has_zero_entries() {
    // Sanity check — a real `SessionRegistry::new()` should report 0 entries.
    let registry: SessionRegistry<1, 4, 2> = SessionRegistry::new();
    assert_eq!(registry.iter().count(), 0);
}

#[test]
fn option_session_state_size_vs_session_state() {
    use core::mem::size_of;
    // If equal: niche-based representation (Option uses an unused bit
    // pattern within SessionState). If different: tagged representation
    // (Option adds a discriminant word). Either way is informative.
    use GatoMQTT::session::state::SessionState;
    let s_some = size_of::<Option<SessionState<4, 2>>>();
    let s_state = size_of::<SessionState<4, 2>>();
    println!("size_of::<Option<SessionState<4, 2>>>() = {s_some}");
    println!("size_of::<SessionState<4, 2>>()         = {s_state}");
    println!(
        "diff = {} (0 → niche, >0 → tagged)",
        s_some.saturating_sub(s_state)
    );
}
