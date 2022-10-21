use slab::Slab;
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
pub struct Reader<S: Subscriber + for<'a> LookupSpan<'a>> {
    /// The channel reciever of log messages.
    rx: mpsc::Receiver<Vec<u8>, WithCapacity>,

    /// Shared state for this stream.
    ///
    /// Currently, the shared state contains a counter of how many messages have
    /// been dropped because the channel was at capacity.
    shared: Arc<Shared>,

    /// Index in the slab of writers on the tx side of the stream.
    ///
    /// This is used to remove the stream when the reader is dropped.
    idx: usize,

    /// The handle for modifying the log stream layer.
    ///
    /// This is used to remove the stream when the reader is dropped.
    handle: StreamHandle<S>,
}

/// A handle for starting new log streams, and removing old ones.
#[derive(Debug)]

pub struct StreamHandle<S> {
    handle: reload::Handle<StreamLayer<S>, S>,

    /// The total capacity of the channel.
    ///
    /// This determines how many events can be buffered before new events are
    /// dropped.
    channel_capacity: usize,

    /// Configuration for the [reuse of individual buffers][recycle].
    ///
    /// This determines the initial size of new buffers in the log channel, and
    /// the maximum size of "idle" (unused) buffers. To minimize reallocations,
    /// buffers are typically cleared in place when reused, retaining any
    /// allocated capacity from previous log events. However, to avoid unbounded
    /// memory use, we place an upper bound on "idle" capacity. If a buffer
    /// grows past that upper bound while a log event is being formatted to it,
    /// it will be shrank back to the upper bound capacity when returned to the
    /// channel.
    ///
    /// [recycle]: https://docs.rs/thingbuf/latest/thingbuf/recycling/struct.WithCapacity.html
    channel_settings: WithCapacity,
}

/// A layer that formats spans and events and writes them to the currently
/// active log streams.
///
/// This consists of a `Slab` of `Writer`s, so that the storage for `Writer`s
/// can be efficiently reused as log streaming requests end and new requests are
/// initiated.
#[derive(Debug)]
pub(crate) struct WriterLayer<S> {
    writers: Slab<Writer<S>>,
}

/// A per-layer filter for a log streaming layer.
///
/// This type is used to interact with `tracing-subscriber`'s [per-layer
/// filtering] system. In an ideal world, we wouldn't need this type. Instead,
/// we would simply have a `Vec` of `Filtered<WriterLayer>`s. However, this
/// isn't currently possible due to limitations in how `tracing`'s per-layer
/// filtering works (see [#2101] and [#1629]): because of how filter IDs are
/// currently generated, layers with per-layer filters cannot easily be added
/// and removed dynamically.
///
/// We work around this limitation by implementing a single per-layer filter
/// and a single formatting layer, which consist of a `Vec` of filters and a
/// `Vec` of filters and formatters, respectively. The filter layer will enable
/// a span or event if *any* of the currently active log streams care about it,
/// and when that span or event is recorded, the `Arc` clone of that filter in
/// the formatter layer is used to determine whether it should actually be
/// recorded by that stream.
///
/// This design is kind of unfortunate, so it can hopefully be removed once
/// these issues are resolved upstream.
///
/// [per-layer filtering]: https://docs.rs/tracing-subscriber/latest/tracing_subscriber/layer/index.html#per-layer-filtering
/// [#2101]: https://github.com/tokio-rs/tracing/issues/2101
/// [#1629]: https://github.com/tokio-rs/tracing/issues/1692
#[derive(Debug, Default)]
pub(crate) struct StreamFilter {
    filters: Vec<Weak<EnvFilter>>,
}

type StreamLayer<S> = Filtered<WriterLayer<S>, StreamFilter, S>;

/// A writer for an individual log stream.
///
/// The `WriterLayer` type consists of a `Slab` containing a `Writer` for each
/// currently active log streaming request.
#[derive(Debug)]
struct Writer<S> {
    // XXX(eliza): having to duplicate the filters here and in the
    // `StreamFilter` type is quite unfortunate. Ideally, we would just have a
    // single `Vec` of `Filtered` layers, but this doesn't play nice with filter
    // reloading (see https://github.com/tokio-rs/tracing/issues/1629).
    //
    // It's possible this will be easier in the future:
    // https://github.com/tokio-rs/tracing/issues/2101
    filter: Arc<EnvFilter>,
    writer: FmtLayer<S>,
}

type FmtLayer<S> = fmt::Layer<S, format::JsonFields, format::Format<format::Json, SystemTime>, Tx>;

/// The sender side of a log streaming channel.
#[derive(Debug)]
struct Tx {
    tx: mpsc::Sender<Vec<u8>, WithCapacity>,
    shared: Arc<Shared>,
}

/// A log line currently in the process of being written.
///
/// When this is dropped, the log line is sent to the receiver side of the
/// channel.
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
            writers: Slab::new(),
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
    ) -> Result<Reader<S>, Box<dyn std::error::Error + Send + Sync>> {
        let filter = Arc::new(filter);
        let (tx, rx) = mpsc::with_recycle(self.channel_capacity, self.channel_settings.clone());
        let shared = Arc::new(Shared::default());
        let mut idx = 0;
        self.handle.modify(|layer| {
            let shared = shared.clone();
            layer.filter_mut().filters.push(Arc::downgrade(&filter));
            idx = layer.inner_mut().add_stream(filter, Tx { tx, shared });
        })?;

        Ok(Reader {
            rx,
            shared,
            idx,
            handle: self.clone(),
        })
    }

    fn remove_stream(&self, idx: usize) {
        tracing::trace!(idx, "Removing log stream...");
        // XXX(eliza): would be nice if `modify` could return a value...`
        let mut did_remove = false;
        let removed = self
            .handle
            .modify(|layer| {
                did_remove = layer.inner_mut().writers.try_remove(idx).is_some();
                layer.filter_mut().filters.retain(|f| f.upgrade().is_some())
            })
            .map(|_| did_remove);
        tracing::trace!(idx, ?removed, "Removed log stream");
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

impl<S> Reader<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    pub async fn next_line(&self) -> Option<RecvRef<'_, Vec<u8>>> {
        self.rx.recv_ref().await
    }

    /// Returns the number of log lines that were dropped since the last time
    /// this method was called.
    ///
    /// Calling this method resets the counter.
    pub fn take_dropped_count(&self) -> usize {
        self.shared.dropped_logs.swap(0, Ordering::Acquire)
    }
}

impl<S> Drop for Reader<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn drop(&mut self) {
        self.handle.remove_stream(self.idx)
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

impl<'a> MakeWriter<'a> for Tx {
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
    fn add_stream(&mut self, filter: Arc<EnvFilter>, tx: Tx) -> usize {
        let fmt = fmt::layer()
            .json()
            .with_current_span(false)
            .with_span_list(true)
            .with_thread_ids(true)
            .with_writer(tx);

        self.writers.insert(Writer {
            filter,
            writer: fmt,
        })
    }

    fn writers_for<'a>(
        &'a self,
        meta: &'a tracing::Metadata<'_>,
        ctx: &'a Context<'_, S>,
    ) -> impl Iterator<Item = &'a FmtLayer<S>> + 'a {
        self.writers
            .iter()
            .filter_map(|(_, Writer { filter, writer })| {
                filter.enabled(meta, ctx.clone()).then_some(writer)
            })
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
