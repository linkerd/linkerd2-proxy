use std::{
    io,
    sync::{Arc, Weak, atomic::{AtomicUsize, Ordering}},
};
use thingbuf::{
    mpsc::{self, SendRef},
    recycle::WithCapacity,
};
use tracing_subscriber::{
    filter::{Filtered, EnvFilter,
    fmt::{format, time::SystemTime, writer::MakeWriter, Layer},
};

#[derive(Debug)]
pub struct StreamLayer<S> {
    filters: Vec<Arc<EnvFilter>>,
    writers: Vec<FmtLayer<S>>,
}

pub struct StreamFilter {
    filters: Vec<Weak<EnvFilter>>,
}

pub struct Reader {
    rx: mpsc::Receiver<String, WithCapacity>,
    shared: Arc<Shared>,
}

type FmtLayer<S> = Layer<S, format::JsonFields, format::Format<format::Json, SystemTime>, Writer>;

struct Writer {
    tx: mpsc::Sender<String, WithCapacity>,
    shared: Arc<Shared>,
}

struct Line<'a>(Option<SendRef<'a, String>>);

struct Shared {
    dropped_logs: AtomicUsize,
}

// === impl Line ===

impl<'a> io::Write for Line<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(ref mut ine) = self.0 {
            return line.write(buf);
        }
        // no channel capacity; drop the line, but pretend it succeeded
        Ok(buf.len())
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
{

}