//! Divan timing benchmarks for pub/sub on a multi-thread runtime. Every
//! subscriber has a parked reader task (as a socket writer would), queues are
//! drained continuously, and a fan-out is timed until the *last* subscriber
//! has received the frame. Payloads are 500 B; 16 topics exist. Allocation
//! counts live in `pubsub_alloc.rs` (the profiling allocator skews timings).
//! Numbers are recorded in `docs/pubsub.md`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use divan::{Bencher, black_box};
use stakit_router::{ClientAction, Router, Session};
use tokio::runtime::Runtime;
use tokio::sync::Notify;

fn main() {
    divan::main();
}

struct Note;
impl ClientAction for Note {
    type Params = String;
    type Return = ();
    const NAME: &'static str = "note";
}

const TOPICS: usize = 16;

/// Counts frames delivered to subscribers; wakes the timer when it hits `goal`.
struct Delivered {
    count: AtomicUsize,
    goal: AtomicUsize,
    done: Notify,
}

impl Delivered {
    fn arm(&self, goal: usize) {
        self.count.store(0, Ordering::SeqCst);
        self.goal.store(goal, Ordering::SeqCst);
    }

    fn record(&self) {
        let seen = self.count.fetch_add(1, Ordering::AcqRel) + 1;
        if seen == self.goal.load(Ordering::Acquire) {
            self.done.notify_one();
        }
    }
}

/// A router with sessions on `TOPICS` topics, each read by a spawned task.
struct Fixture {
    runtime: Runtime,
    router: Arc<Router<(), ()>>,
    delivered: Arc<Delivered>,
    _sessions: Vec<Session<(), ()>>,
}

impl Fixture {
    /// `per_topic[t]` subscribers on topic `room:t`.
    fn new(per_topic: &[usize]) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let router = Arc::new(Router::builder().ctx(()).build());
        let delivered = Arc::new(Delivered {
            count: AtomicUsize::new(0),
            goal: AtomicUsize::new(usize::MAX),
            done: Notify::new(),
        });
        let _context = runtime.enter();
        let mut sessions = Vec::new();
        for (topic, count) in per_topic.iter().enumerate() {
            for _ in 0..*count {
                let mut session = router.session(());
                let mut outgoing = session.outgoing().unwrap();
                session.subscribe(&format!("room:{topic}")).unwrap();
                let delivered = Arc::clone(&delivered);
                runtime.spawn(async move {
                    while let Some(frame) = outgoing.recv().await {
                        black_box::<Bytes>(frame);
                        delivered.record();
                    }
                });
                sessions.push(session);
            }
        }
        Self {
            runtime,
            router,
            delivered,
            _sessions: sessions,
        }
    }
}

fn payload() -> String {
    "x".repeat(500)
}

/// One publish to a topic with `n` subscribers (15 other topics have 10
/// each), timed until all `n` readers have it.
#[divan::bench(args = [1, 100, 1_000, 10_000], sample_count = 50)]
fn publish_fan_out(bencher: Bencher<'_, '_>, n: usize) {
    let mut per_topic = vec![10; TOPICS];
    per_topic[0] = n;
    let fixture = Fixture::new(&per_topic);
    let hub = fixture.router.hub().clone();
    let params = payload();
    bencher.bench_local(|| {
        fixture.runtime.block_on(async {
            fixture.delivered.arm(n);
            hub.publish::<Note>("room:0", &params).await.unwrap();
            fixture.delivered.done.notified().await;
        });
    });
}

/// 16 concurrent publishers, one per topic (100 subscribers each), timed
/// until all 1 600 readers have their frame.
#[divan::bench(sample_count = 50)]
fn publish_16_topics_concurrently(bencher: Bencher<'_, '_>) {
    let fixture = Fixture::new(&[100; TOPICS]);
    let hub = fixture.router.hub().clone();
    let params = Arc::new(payload());
    bencher.bench_local(|| {
        fixture.runtime.block_on(async {
            fixture.delivered.arm(100 * TOPICS);
            let publishers: Vec<_> = (0..TOPICS)
                .map(|topic| {
                    let (hub, params) = (hub.clone(), Arc::clone(&params));
                    tokio::spawn(async move {
                        hub.publish::<Note>(&format!("room:{topic}"), &params)
                            .await
                            .unwrap();
                    })
                })
                .collect();
            for publisher in publishers {
                publisher.await.unwrap();
            }
            fixture.delivered.done.notified().await;
        });
    });
}

/// Subscribe + unsubscribe to a fresh topic (creates and drops the topic
/// state, i.e. the worst case: broker interest flips both ways).
#[divan::bench]
fn subscribe_unsubscribe(bencher: Bencher<'_, '_>) {
    let fixture = Fixture::new(&[0]);
    let _context = fixture.runtime.enter();
    let session = fixture.router.session(());
    bencher.bench_local(|| {
        session.subscribe(black_box("room:42")).unwrap();
        session.unsubscribe(black_box("room:42"))
    });
}

/// Subscribe + unsubscribe on a topic that already has 1000 subscribers
/// (topic state stays alive; index insert/remove only).
#[divan::bench]
fn subscribe_unsubscribe_busy_topic(bencher: Bencher<'_, '_>) {
    let fixture = Fixture::new(&[1_000]);
    let _context = fixture.runtime.enter();
    let session = fixture.router.session(());
    bencher.bench_local(|| {
        session.subscribe(black_box("room:0")).unwrap();
        session.unsubscribe(black_box("room:0"))
    });
}
