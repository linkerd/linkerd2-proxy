use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Weak,
    },
};
use thingbuf::{
    mpsc::{self, RecvRef, SendRef},
    recycling::WithCapacity,
};
use tracing::{
    span,
    subscriber::{Interest, Subscriber},
};
use tracing_subscriber::{
    filter::{EnvFilter, Filtered},
    fmt::{self, format, time::SystemTime, writer::MakeWriter},
    layer::{Context, Filter, Layer},
    registry::LookupSpan,
    reload,
};

/// The receiver end of a log stream.
#[derive(Debug)]
pub struct Reader {
    rx: mpsc::Receiver<Vec<u8>, WithCapacity>,
    #[allow(dead_code)] // TODO(eliza): use this
    shared: Arc<Shared>,
}

pub(crate) type StreamLayer<S> = Filtered<WriterLayer<S>, StreamFilter, S>;

/// A handle for starting new log streams.
#[derive(Debug)]

pub struct StreamHandle<S> {
    handle: reload::Handle<StreamLayer<S>, S>,
    channel_capacity: usize,
    channel_settings: WithCapacity,
}

#[derive(Debug)]
pub(crate) struct WriterLayer<S> {
    // XXX(eliza): having to duplicate the filters here and in the
    // `StreamFilter` type is quite unfortunate. Ideally, we would just have a
    // single `Vec` of `Filtered` layers, but this doesn't play nice with filter
    // reloading (see https://github.com/tokio-rs/tracing/issues/1629).
    //
    // It's possible this will be easier in the future:
    // https://github.com/tokio-rs/tracing/issues/2101
    filters: Vec<Arc<EnvFilter>>,
    writers: Vec<FmtLayer<S>>,
}

#[derive(Debug, Default)]
pub(crate) struct StreamFilter {
    filters: Vec<Weak<EnvFilter>>,
}

type FmtLayer<S> =
    fmt::Layer<S, format::JsonFields, format::Format<format::Json, SystemTime>, Writer>;

#[derive(Debug)]
struct Writer {
    tx: mpsc::Sender<Vec<u8>, WithCapacity>,
    shared: Arc<Shared>,
}

struct Line<'a>(Option<SendRef<'a, Vec<u8>>>);

#[derive(Debug, Default)]
struct Shared {
    dropped_logs: AtomicUsize,
}

// === impl StreamHandle ===

impl<S> StreamHandle<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    pub(crate) fn new() -> (Self, reload::Layer<StreamLayer<S>, S>) {
        let layer = WriterLayer {
            filters: Vec::new(),
            writers: Vec::new(),
        };
        let layer = layer.with_filter(StreamFilter::default());
        let (layer, handle) = reload::Layer::new(layer);
        let handle = Self {
            handle,
            // TODO(eliza): make these configurable, or at least tune them a bit...
            channel_capacity: 512,
            // log lines probably won't ever be over 1kb in length, but ensure
            // idle capacity is bounded so we don't have unbounded memory growth.
            channel_settings: WithCapacity::new().with_max_capacity(1024),
        };
        (handle, layer)
    }

    pub fn add_stream(
        &self,
        filter: EnvFilter,
    ) -> Result<Reader, Box<dyn std::error::Error + Send + Sync>> {
        let filter = Arc::new(filter);
        let (tx, rx) = mpsc::with_recycle(self.channel_capacity, self.channel_settings.clone());
        let shared = Arc::new(Shared::default());
        self.handle.modify(|layer| {
            let shared = shared.clone();
            layer.filter_mut().filters.push(Arc::downgrade(&filter));
            layer.inner_mut().add_stream(filter, Writer { tx, shared });
        })?;

        Ok(Reader { rx, shared })
    }
}

impl<S> Clone for StreamHandle<S> {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            channel_capacity: self.channel_capacity,
            channel_settings: self.channel_settings.clone(),
        }
    }
}

// === impl Reader ===

impl Reader {
    pub async fn next_line(&self) -> Option<RecvRef<'_, Vec<u8>>> {
        self.rx.recv_ref().await
    }
}

// === impl Line ===

impl<'a> io::Write for Line<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(ref mut line) = self.0 {
            line.extend_from_slice(buf)
        }
        // no channel capacity; drop the line, but pretend it succeeded
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// === impl Writer ===

impl<'a> MakeWriter<'a> for Writer {
    type Writer = Line<'a>;

    fn make_writer(&'a self) -> Line<'a> {
        match self.tx.try_send_ref() {
            Ok(line) => Line(Some(line)),
            Err(_) => {
                self.shared.dropped_logs.fetch_add(1, Ordering::Relaxed);
                Line(None)
            }
        }
    }
}

// === impl WriterLayer ===

impl<S> WriterLayer<S> {
    fn add_stream(&mut self, filter: Arc<EnvFilter>, tx: Writer) {
        debug_assert_eq!(
            self.filters.len(),
            self.writers.len(),
            "writer and filter must have the same index"
        );
        self.filters.push(filter);
        let fmt = fmt::layer()
            .json()
            .with_current_span(false)
            .with_span_list(true)
            .with_thread_ids(true)
            .with_writer(tx);
        self.writers.push(fmt);
    }

    fn writers_for<'a>(
        &'a self,
        meta: &'a tracing::Metadata<'_>,
        ctx: &'a Context<'_, S>,
    ) -> impl Iterator<Item = &'a FmtLayer<S>> + 'a {
        self.filters
            .iter()
            .zip(self.writers.iter())
            .filter_map(|(filter, writer)| filter.enabled(meta, ctx.clone()).then(|| writer))
    }
}

impl<S> Layer<S> for WriterLayer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        for writer in self.writers_for(event.metadata(), &ctx) {
            writer.on_event(event, ctx.clone())
        }
    }

    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        for writer in self.writers_for(attrs.metadata(), &ctx) {
            writer.on_new_span(attrs, id, ctx.clone())
        }
    }
    // TODO(eliza): add `on_enter`/`on_exit`/`on_close` impls if
    // we care about being able to support span hooks here?
}

// === impl Filter ===

impl StreamFilter {
    fn filters(&self) -> impl Iterator<Item = Arc<EnvFilter>> + '_ {
        self.filters.iter().filter_map(Weak::upgrade)
    }
}

impl<S: Subscriber> Filter<S> for StreamFilter {
    fn enabled(&self, meta: &tracing::Metadata<'_>, ctx: &Context<'_, S>) -> bool {
        self.filters()
            .any(|filter| Filter::enabled(&*filter, meta, ctx))
    }

    fn callsite_enabled(&self, meta: &'static tracing::Metadata<'static>) -> Interest {
        let mut interest = Interest::never();
        for filter in self.filters() {
            let new_interest = Layer::<S>::register_callsite(&*filter, meta);
            if (interest.is_sometimes() && new_interest.is_always())
                || (interest.is_never() && !new_interest.is_never())
            {
                interest = new_interest;
            }
        }
        interest
    }

    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        for filter in self.filters() {
            filter.on_new_span(attrs, id, ctx.clone())
        }
    }

    fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
        for filter in self.filters() {
            filter.on_enter(id, ctx.clone())
        }
    }

    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        for filter in self.filters() {
            filter.on_exit(id, ctx.clone())
        }
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        for filter in self.filters() {
            filter.on_close(id.clone(), ctx.clone())
        }
    }
}
