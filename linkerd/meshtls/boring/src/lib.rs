#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

mod client;
pub mod creds;
mod server;

pub use self::{
    client::{ClientIo, Connect, ConnectFuture, NewClient},
    server::{Server, ServerIo, TerminateFuture},
};
use linkerd_error::Result;

/// Encodes a list of ALPN protocols into a slice of bytes.
///
/// `boring` requires that the list of protocols be encoded in the wire format.
#[allow(dead_code)]
fn serialize_alpn(protocols: &[Vec<u8>]) -> Result<Vec<u8>> {
    // Allocate a buffer to hold the encoded protocols.
    let mut bytes = {
        // One additional byte for each protocol's length prefix.
        let cap = protocols.len() + protocols.iter().map(Vec::len).sum::<usize>();
        Vec::with_capacity(cap)
    };

    // Encode each protocol as a length-prefixed string.
    for p in protocols {
        if p.len() > 255 {
            return Err("ALPN protocols must be less than 256 bytes".into());
        }
        bytes.push(p.len() as u8);
        bytes.extend(p);
    }

    Ok(bytes)
}

#[cfg(test)]
#[test]
fn test_serialize_alpn() {
    assert_eq!(serialize_alpn(&[b"h2".to_vec()]).unwrap(), b"\x02h2");
    assert_eq!(
        serialize_alpn(&[b"h2".to_vec(), b"http/1.1".to_vec()]).unwrap(),
        b"\x02h2\x08http/1.1"
    );

    assert!(serialize_alpn(&[(0..255).collect()]).is_ok());
    assert!(serialize_alpn(&[(0..=255).collect()]).is_err());
}
