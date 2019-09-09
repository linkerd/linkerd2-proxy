use bytes::Bytes;
use rand::Rng;
use std::collections::HashMap;
use std::fmt;
use std::time::SystemTime;
use tokio::sync::mpsc;

pub mod layer;
mod propagation;

pub use layer::layer;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Default)]
pub struct Id(Vec<u8>);

#[derive(Debug, Default)]
pub struct Flags(u8);

#[derive(Debug)]
pub struct Span {
    pub trace_id: Id,
    pub span_id: Id,
    pub parent_id: Id,
    pub span_name: String,
    pub start: SystemTime,
    pub end: SystemTime,
    pub labels: HashMap<String, String>,
}

pub trait SpanSink {
    fn try_send(&mut self, span: Span) -> Result<(), Error>;
}

impl SpanSink for mpsc::Sender<Span> {
    fn try_send(&mut self, span: Span) -> Result<(), Error> {
        self.try_send(span).map_err(Into::into)
    }
}

// === impl Id ===

impl Id {
    fn new(len: usize) -> Self {
        let mut rng = rand::thread_rng();
        let mut bytes = vec![0; len];
        rng.fill(bytes.as_mut_slice());
        Self(bytes)
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl AsRef<Vec<u8>> for Id {
    fn as_ref(&self) -> &Vec<u8> {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0.iter() {
            write!(f, "{:02x?}", b)?;
        }
        Ok(())
    }
}

impl From<Bytes> for Id {
    fn from(buf: Bytes) -> Self {
        Id(buf.to_vec())
    }
}

// === impl Flags ===

impl Flags {
    pub fn is_sampled(&self) -> bool {
        self.0 & 1 == 1
    }
}

impl fmt::Display for Flags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x?}", self.0)
    }
}

impl From<Bytes> for Flags {
    fn from(buf: Bytes) -> Self {
        Flags(buf[0])
    }
}
