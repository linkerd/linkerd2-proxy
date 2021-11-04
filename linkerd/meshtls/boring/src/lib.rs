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
fn serialize_alpn(protos: &[Vec<u8>]) -> Result<Vec<u8>> {
    let mut bytes = {
        // One additional byte for each protocol's length prefix.
        let cap = protos.len() + protos.iter().map(|p| p.len()).sum::<usize>();
        Vec::with_capacity(cap)
    };

    for proto in protos {
        if proto.len() > 255 {
            return Err("ALPN protocol must be less than 255 bytes".into());
        }
        bytes.push(proto.len() as u8);
        bytes.extend(&*proto);
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
}
