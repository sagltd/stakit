//! Memory budget for idle sessions at the router layer (excludes the socket
//! and its buffers): 10k authenticated sessions (user + device channels, a
//! realistic `Arc`'d request context) with one more subscription each, on a
//! driven runtime; net bytes still allocated / 10k. Its own test binary — the
//! counting allocator is global.

use std::alloc::System;
use std::sync::Arc;

use stakit_router::Router;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[path = "support/fixtures.rs"]
mod fixtures;
use fixtures::{Ctx, idle_session};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

/// Budget per idle session; see `docs/pubsub.md` for the measurement. About
/// 2× the measured footprint: allocator/runtime variance passes, a doubled
/// footprint fails.
const BUDGET_BYTES: usize = 12 * 1024;

const SESSIONS: usize = 10_000;

#[test]
fn idle_session_stays_within_memory_budget() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let router = Arc::new(Router::<(), Ctx>::builder().ctx(()).build());

    let region = Region::new(GLOBAL);
    let open = runtime.block_on(async {
        let open: Vec<_> = (0..SESSIONS)
            .map(|index| idle_session(&router, index))
            .collect();
        // Let the spawned timer tasks run to their first await, as they would.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        open
    });
    let change = region.change();
    let net = change.bytes_allocated.cast_signed() - change.bytes_deallocated.cast_signed()
        + change.bytes_reallocated;
    let retained = usize::try_from(net).unwrap_or(0);
    let per_session = retained / SESSIONS;
    println!("idle session ≈ {per_session} bytes ({retained} bytes for {SESSIONS})");
    assert!(
        per_session < BUDGET_BYTES,
        "idle session uses {per_session} bytes"
    );
    assert_eq!(router.hub().subscriber_count("room:shared"), SESSIONS);
    assert_eq!(router.hub().subscriber_count("user:id=user7"), 1);
    drop(open);
    assert_eq!(router.hub().connection_count(), 0);
}
