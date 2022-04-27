use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Weak,
    },
};
use thingbuf::{
    mpsc::{self, SendRef},
    recycling::WithCapacity,
};
use tracing::{Event, Subscriber};
use tracing_subscriber::{
    filter::{EnvFilter, Filtered},
    fmt::{self, format, time::SystemTime, writer::MakeWriter},
    layer::{Context, Layer},
    registry::LookupSpan,
};

#[derive(Debug)]
pub struct StreamLayer<S> {
    filters: Vec<Arc<EnvFilter>>,
    writers: Vec<FmtLayer<S>>,
}

#[derive(Debug)]
pub struct StreamFilter {
    filters: Vec<Weak<EnvFilter>>,
}

pub struct Reader {
    rx: mpsc::Receiver<String, WithCapacity>,
    shared: Arc<Shared>,
}

type FmtLayer<S> =
    fmt::Layer<S, format::JsonFields, format::Format<format::Json, SystemTime>, Writer>;

#[derive(Debug)]
struct Writer {
    tx: mpsc::Sender<Vec<u8>, WithCapacity>,
    shared: Arc<Shared>,
}

struct Line<'a>(Option<SendRef<'a, Vec<u8>>>);

#[derive(Debug)]
struct Shared {
    dropped_logs: AtomicUsize,
}

// === impl Line ===

impl<'a> io::Write for Line<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(ref mut line) = self.0 {
            line.copy_from_slice(buf);
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

impl<S> Layer<S> for StreamLayer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        todo!("eliz")
    }
}
