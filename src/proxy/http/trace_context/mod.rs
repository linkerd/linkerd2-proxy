use bytes::Bytes;
use rand::Rng;
use std::fmt;
use std::time::SystemTime;

pub mod layer;
mod propagation;

pub use layer::layer;

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
