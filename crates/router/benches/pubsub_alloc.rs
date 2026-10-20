//! Allocation profile (divan `AllocProfiler`) for pub/sub: what opening an
//! idle, authenticated session costs, and what one publish allocates. Timings
//! here are skewed by the profiling allocator — use `pubsub.rs` for those.

use std::cell::RefCell;
use std::sync::Arc;

use divan::{AllocProfiler, Bencher, black_box};
use futures::executor::block_on;
use stakit_router::{ClientAction, Router};

#[path = "../tests/support/fixtures.rs"]
mod fixtures;
use fixtures::{Ctx, idle_session};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

fn main() {
    divan::main();
}

struct Note;
impl ClientAction for Note {
    type Params = String;
    type Return = ();
    const NAME: &'static str = "note";
}

/// A runtime (never driven) and a router with `Ctx` requests.
fn fixture() -> (tokio::runtime::Runtime, Arc<Router<(), Ctx>>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    (runtime, Arc::new(Router::builder().ctx(()).build()))
}

/// Opening an authenticated idle session (user + device channels, an `Arc`'d
/// request context) with one more subscription.
#[divan::bench]
fn open_idle_session(bencher: Bencher<'_, '_>) {
    let (runtime, router) = fixture();
    let _context = runtime.enter();
    let mut index = 0;
    bencher.bench_local(|| {
        index += 1;
        idle_session(&router, index)
    });
}

/// One 500 B publish to 100 subscribers (queues drained before each one).
#[divan::bench(sample_size = 1)]
fn publish_100_subscribers(bencher: Bencher<'_, '_>) {
    let (runtime, router) = fixture();
    let _context = runtime.enter();
    let open = RefCell::new(
        (0..100)
            .map(|index| idle_session(&router, index))
            .collect::<Vec<_>>(),
    );
    let hub = router.hub().clone();
    let params = "x".repeat(500);
    bencher
        .with_inputs(|| {
            for (_, outgoing) in open.borrow_mut().iter_mut() {
                while outgoing.try_recv().is_ok() {}
            }
        })
        .bench_local_values(|()| block_on(hub.publish::<Note>("room:shared", black_box(&params))));
}
