#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod export;
mod service;

pub use self::service::TraceContext;
use bytes::Bytes;
use opentelemetry::{KeyValue, SpanId, TraceId};
use std::fmt;
use std::time::SystemTime;
use thiserror::Error;

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct Id(Vec<u8>);

#[derive(Debug, Error)]
#[error("ID '{:?} should have {} bytes, but it has {}", self.id, self.expected_size, self.actual_size)]
pub struct IdLengthError {
    id: Vec<u8>,
    expected_size: usize,
    actual_size: usize,
}

impl Id {
    pub fn into_bytes<const N: usize>(self) -> Result<[u8; N], IdLengthError> {
        self.as_ref().try_into().map_err(|_| {
            let bytes: Vec<u8> = self.into();
            IdLengthError {
                expected_size: N,
                actual_size: bytes.len(),
                id: bytes,
            }
        })
    }
}

#[derive(Debug, Default)]
pub struct Flags(u8);

#[derive(Debug, Error)]
#[error("insufficient bytes when decoding binary header")]
pub struct InsufficientBytes;

#[derive(Debug)]
pub struct Span {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent_id: SpanId,
    pub span_name: String,
    pub start: SystemTime,
    pub end: SystemTime,
    pub labels: Vec<KeyValue>,
}

// === impl Id ===

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
            write!(f, "{b:02x?}")?;
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
