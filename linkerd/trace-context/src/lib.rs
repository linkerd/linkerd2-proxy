#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
#![forbid(unsafe_code)]

mod propagation;
mod service;

pub use self::service::TraceContext;
use bytes::Bytes;
use linkerd_error::Error;
use rand::Rng;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt;
use std::time::SystemTime;
use thiserror::Error;

const SPAN_ID_LEN: usize = 8;

#[derive(Debug, Default)]
pub struct Id(Vec<u8>);

#[derive(Debug, Default)]
pub struct Flags(u8);

#[derive(Debug, Error)]
#[error("insufficient bytes when decoding binary header")]
pub struct InsufficientBytes;

#[derive(Debug)]
pub struct Span {
    pub trace_id: Id,
    pub span_id: Id,
    pub parent_id: Id,
    pub span_name: String,
    pub start: SystemTime,
    pub end: SystemTime,
    pub labels: HashMap<&'static str, String>,
}

pub trait SpanSink {
    fn is_enabled(&self) -> bool;

    fn try_send(&mut self, span: Span) -> Result<(), Error>;
}

impl<K: SpanSink> SpanSink for Option<K> {
    #[inline]
    fn is_enabled(&self) -> bool {
        self.as_ref().map(SpanSink::is_enabled).unwrap_or(false)
    }

    #[inline]
    fn try_send(&mut self, span: Span) -> Result<(), Error> {
        self.as_mut().expect("Must be enabled").try_send(span)
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

impl From<Id> for Vec<u8> {
    fn from(Id(bytes): Id) -> Vec<u8> {
        bytes
    }
}

impl AsRef<[u8]> for Id {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
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
