use metrics::{latency, FmtLabels, FmtMetric, Histogram};
use proxy::http::insert;
use std::{
    fmt,
    sync::{
        atomic::{self, AtomicUsize, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Instant,
};

/// A single handle time histogram.
///
/// Higher-level code will use this to represent a single set of labels for
/// handle-time metrics.
#[derive(Debug, Clone)]
pub struct Scope(Arc<Shared>);

/// A layer that inserts a `Tracker` into each request passing through it.
#[derive(Debug, Clone)]
pub struct InsertTracker(Arc<Shared>);

/// A request extension that, when dropped, records the time elapsed since it
/// was created.
#[derive(Debug)]
pub struct Tracker {
    shared: Arc<Shared>,
    idx: usize,
    t0: Instant,
}

#[derive(Debug)]
struct Shared {
    // NOTE: this is inside a `Mutex` since recording a latency requires a mutable
    // reference to the histogram. In the future, we could consider making the
    // histogram counters `AtomicU64, so that the histogram could be updated
    // with an immutable reference. Then, the mutex could be removed.
    histogram: Mutex<Histogram<latency::Us>>,
    /// Stores the state of currently active `Tracker`s.
    counts: RwLock<Vec<Count>>,
    /// The index of the most recently finished counter.
    ///
    /// When a new counter is needed, the counter at this index will be used,
    /// and that counter's next index will be set as the head. of the free
    /// list.
    ///
    /// When an active counter completes, this will be set to its index, and
    /// the previous value will become the freed counter's next pointer.
    idle_head: AtomicUsize,
}

/// Counts the number of times a request has been cloned or dropped.
///
/// Since requests may be cloned for retries, we must count the number of clones
/// currently alive to ensure that the handle time for that request has
/// completed fully.
#[derive(Debug)]
struct Count {
    /// The number of currently active `Tracker`s for this request..
    ///
    /// When a request is initially received, there will be one `Tracker` in its
    /// `Extensions` map. If the request is cloned for retries, the `Tracker`
    /// will be cloned, incrementing this count. Dropping a `Tracker` decrements
    /// this count, and when it reaches 0, the handle time is recorded.
    clones: AtomicUsize,
    /// Index of the next free counter.
    next_idle: AtomicUsize,
}

impl insert::Lazy<Tracker> for InsertTracker {
    fn value(&self) -> Tracker {
        self.0.clone().tracker()
    }
}

// ===== impl Scope =====

impl Scope {
    pub fn new() -> Self {
        Scope(Arc::new(Shared::new()))
    }

    pub fn layer(&self) -> insert::Layer<InsertTracker, Tracker> {
        insert::Layer::new(InsertTracker(self.0.clone()))
    }
}

impl FmtMetric for Scope {
    const KIND: &'static str = <Histogram<latency::Us> as FmtMetric>::KIND;

    fn fmt_metric<N: fmt::Display>(&self, f: &mut fmt::Formatter<'_>, name: N) -> fmt::Result {
        if let Ok(hist) = self.0.histogram.lock() {
            hist.fmt_metric(f, name)?;
        }
        Ok(())
    }

    fn fmt_metric_labeled<N, L>(
        &self,
        f: &mut fmt::Formatter<'_>,
        name: N,
        labels: L,
    ) -> fmt::Result
    where
        N: fmt::Display,
        L: FmtLabels,
    {
        if let Ok(hist) = self.0.histogram.lock() {
            hist.fmt_metric_labeled(f, name, labels)?;
        }
        Ok(())
    }
}

// ===== impl InsertTracker =====

impl Clone for Tracker {
    fn clone(&self) -> Self {
        self.shared.clone_tracker(self.idx);
        Self {
            shared: self.shared.clone(),
            idx: self.idx,
            t0: self.t0,
        }
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        self.shared.drop_tracker(&*self);
    }
}

impl Shared {
    const INITIAL_RECORDERS: usize = 32;

    fn new() -> Self {
        let mut counts = Vec::with_capacity(Self::INITIAL_RECORDERS);
        Self::add_counts(&mut counts, Self::INITIAL_RECORDERS);
        Self {
            histogram: Mutex::new(Histogram::default()), // TODO(eliza): should we change the bounds here?
            counts: RwLock::new(counts),
            idle_head: AtomicUsize::new(0),
        }
    }

    fn tracker(self: Arc<Self>) -> Tracker {
        let t0 = Instant::now();
        loop {
            let idx = self.idle_head.load(Ordering::Relaxed);
            // This is determined in a scope so that we can move `Self` into the
            // new tracker without doing a second (unecessary) arc bump.
            let acquired = {
                // Do we have any free counts remaining, or must we grow the
                // slab?
                let counts = self
                    .counts
                    .read()
                    .ok()
                    .filter(|counts| idx < counts.len())
                    .unwrap_or_else(|| {
                        // Slow path: if there are no free counts in the
                        // slab, extend it (acquiring a write lock temporarily).
                        self.grow();
                        self.counts.read().unwrap()
                    });

                let next = counts[idx].next_idle.load(Ordering::Acquire);
                // If the counter is still idle, update its ref count & set the
                // free index to point at the next free counter.
                counts[idx].clones.compare_and_swap(0, 1, Ordering::AcqRel) == 0
                    && self.idle_head.compare_and_swap(idx, next, Ordering::AcqRel) == idx
            };

            if acquired {
                return Tracker {
                    shared: self,
                    idx,
                    t0,
                };
            }

            // The counter at `idx` was not actually free! Try again with a
            // fresh index.
            atomic::spin_loop_hint()
        }
    }

    #[inline(never)]
    fn grow(&self) {
        let mut counts = self.counts.write().unwrap();
        let amount = counts.len() * 2;
        counts.reserve(amount);
        Self::add_counts(&mut counts, amount);
    }

    /// Called when a tracker is dropped. This updates the counter of clones for
    /// that request, and records its handle time when the final clone is dropped.
    fn drop_tracker(&self, Tracker { idx, t0, .. }: &Tracker) {
        let panicking = std::thread::panicking();
        let counts = match self.counts.read() {
            Ok(lock) => lock,
            // Avoid double panicking in drop.
            Err(_) if panicking => return,
            Err(e) => panic!("lock poisoned: {:?}", e),
        };
        let idx = *idx;
        let counter = match counts.get(idx) {
            Some(counter) => counter,
            None if panicking => return,
            None => panic!("counts[{:?}] did not exist", idx),
        };

        // If the prior count was 1, it's now 0 and all clones of the request
        // have been fully dropped, so we can now record its handle time.
        if counter.clones.fetch_sub(1, Ordering::Release) == 1 {
            let elapsed = t0.elapsed();

            let mut hist = match self.histogram.lock() {
                Ok(lock) => lock,
                // Avoid double panicking in drop.
                Err(_) if panicking => return,
                Err(e) => panic!("lock poisoned: {:?}", e),
            };

            // Record the handle time for this counter.
            hist.add(elapsed);

            // Link the counter onto the free list by setting  the free-list
            // head to its index, and setting the counter's next pointer to the
            // previous head index.
            let next_idx = self.idle_head.swap(idx, Ordering::AcqRel);
            counter.next_idle.store(next_idx, Ordering::Release);
        }
    }

    /// Called when cloning a tracker into a copy of a request.
    ///
    /// This updates the count of clones for that request.
    fn clone_tracker(&self, idx: usize) {
        let counts = self.counts.read().unwrap();
        let _prev = counts[idx].clones.fetch_add(1, Ordering::Release);
        debug_assert!(_prev > 0, "cannot clone an idle tracker");
    }

    fn add_counts(counts: &mut Vec<Count>, amount: usize) {
        let len = counts.len();
        let new_len = len + amount;
        for i in len..new_len {
            let next_idle = AtomicUsize::new(i + 1);
            counts.push(Count {
                clones: AtomicUsize::new(0),
                next_idle,
            })
        }
    }
}
