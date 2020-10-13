#![deny(warnings, rust_2018_idioms)]

pub use self::layer::TraceContext;
use bytes::Bytes;
use linkerd2_error::Error;
use rand::Rng;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt;
use std::time::SystemTime;
use tokio::sync::mpsc;

pub mod layer;
mod propagation;

const SPAN_ID_LEN: usize = 8;

#[derive(Debug, Default)]
pub struct Id(Vec<u8>);

#[derive(Debug, Default)]
pub struct Flags(u8);

#[derive(Debug)]
pub struct InsufficientBytes;

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
    fn new_span_id<R: Rng>(rng: &mut R) -> Self {
        let mut bytes = vec![0; SPAN_ID_LEN];
        rng.fill(bytes.as_mut_slice());
        Self(bytes)
    }
}

impl Into<Vec<u8>> for Id {
    fn into(self) -> Vec<u8> {
        self.0
    }
}

impl AsRef<[u8]> for Id {
    fn as_ref(&self) -> &[u8] {
        &self.0.as_ref()
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

impl TryFrom<Bytes> for Flags {
    type Error = InsufficientBytes;

    fn try_from(buf: Bytes) -> Result<Self, Self::Error> {
        buf.first().map(|b| Flags(*b)).ok_or(InsufficientBytes)
    }
}

// === impl InsufficientBytes ===

impl std::error::Error for InsufficientBytes {}

impl fmt::Display for InsufficientBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Insufficient bytes when decoding binary header")
    }
}
