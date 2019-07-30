use std::{
    fmt,
    sync::{Arc, atomic::{AtomicUsize, Ordering, self}, Mutex, RwLock},
    time::Instant,
};
use metrics::{Histogram, latency, FmtLabels, FmtMetric};
use proxy::http::insert;

#[derive(Debug, Clone)]
pub struct Scope(Arc<Shared>);

#[derive(Debug)]
struct Shared {
    histogram: Mutex<Histogram<latency::Us>>,
    recorders: RwLock<Vec<Recorder>>,
    next_recorder: AtomicUsize,
}

#[derive(Debug, Clone)]
pub struct InsertTracker(Arc<Shared>);

#[derive(Debug)]
pub struct Tracker {
    shared: Arc<Shared>,
    idx: usize,
    t0: Instant,
}

#[derive(Debug)]
struct Recorder {
    ref_count: AtomicUsize,
    next: AtomicUsize,
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
        if let Ok(hist) = self.0.lock() {
            hist.fmt_metric(f, name)
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
        if let Ok(hist) = self.0.lock() {
            hist.fmt_metric_labeled(f, name, labels)
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
        let mut recorders = Vec::with_capacity(Self::INITIAL_RECORDERS);
        for i in 0..Self::INITIAL_RECORDERS {
            let next = AtomicUsize::new(i + 1);
            recorders.push(Recorder {
                ref_count: AtomicUsize::new(0),
                next,
            })
        }
        Self {
            histogram: Mutex::new(Histogram::default()), // TODO(eliza): should we change the bounds here?
            recorders: RwLock::new(recorders),
            next_recorder: AtomicUsize::new(0),
        }
    }

    fn tracker(self: Arc<Self>) -> Tracker {
        let t0 = Instant::now();
        loop {
            let idx = self.next_recorder.load(Ordering::Relaxed);
            let recorders = self.recorders.read().ok()
                .filter(|recorders| idx < recorders.len())
                .unwrap_or_else(|| {
                    self.grow();
                    self.recorders.read().unwrap()
                });

            let next = recorders[idx].next.load(Ordering::Acquire);

            // Is the recorder at this index actually idle, and is our snapshot
            // of the head index still valid?
            if recorders[idx].ref_count.compare_and_swap(0, 1, Ordering::AcqRel) == 0 &&
                self.next_recorder.compare_and_swap(idx, next, Ordering::AcqRel) == idx
            {
                return Tracker {
                    shared: self,
                    idx,
                    t0
                };
            }

            atomic::spin_loop_hint()
        }
    }

    #[cold]
    #[inline(never)]
    fn grow(&self) {
        let mut recorders = self.recorders.write().unwrap();
        let len = recorders.len();
        let new_len = len * 2;
        recorders.reserve(new_len);
        for i in len..new_len {
            let next = AtomicUsize::new(i + 1);
            recorders.push(Recorder {
                ref_count: AtomicUsize::new(0),
                next,
            })
        }
    }

    fn drop_tracker(&self, Tracker { idx, t0, .. }: &Tracker) {
        let panicking = std::thread::panicking();
        let recorders = match self.recorders.read() {
            Ok(lock) => lock,
            // Avoid double panicking in drop.
            Err(_) if panicking => return,
            Err(e) => panic!("lock poisoned: {:?}", e),
        };
        let idx = *idx;

        let recorder = match recorders.get(idx) {
            Some(recorder) => recorder,
            None if panicking => return,
            None => panic!("recorders[{:?}] did not exist", idx),
        };
        if recorder.ref_count.fetch_sub(1, Ordering::Relaxed) == 0 {
            atomic::fence(Ordering::Acquire);
            let dur = *t0.elapsed();

            let mut hist = match self.histogram.lock() {
                Ok(lock) => lock,
                // Avoid double panicking in drop.
                Err(_) if panicking => return,
                Err(e) => panic!("lock poisoned: {:?}", e),
            };
            hist.add(dur);
            let next_idx = self.next_recorder.swap(idx, Ordering::AcqRel);
            recorder.store(next_idx, Ordering::Release);
        }
    }

    fn clone_tracker(&self, idx: usize) {
        let recorders = self.recorders.read().unwrap();
        let _prev = recorders[idx].ref_count.fetch_add(1, Ordering::Release);
        debug_assert!(_prev > 0, "cannot clone an idle tracker");
    }
}
