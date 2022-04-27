use std::{io, sync::atomic::AtomicUsize};
use thingbuf::mpsc::{self, SendRef};
use tracing_subscriber::fmt::writer::MakeWriter;

pub struct Writer {
    tx: mpsc::Sender<String>,
    shared: Arc<Shared>,
}

pub struct Reader {
    rx: mpsc::Receiver<String>,
    shared: Arc<Shared>,
}

pub struct Line<'a>(SendRef<'a, String>);

// === impl Line ===

impl<'a> io::Write for Line<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
}
struct Shared {
    dropped_logs: AtomicUsize,
}
