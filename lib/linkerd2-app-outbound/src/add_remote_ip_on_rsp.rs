//! Adds `l5d-remote-ip` headers to http::Responses derived from the
//! `remote` of a `Source`.

use super::super::L5D_REMOTE_IP;
use super::Endpoint;
use crate::proxy::http::add_header::{self, response::ResHeader, Layer};
use bytes::Bytes;
use http::header::HeaderValue;

pub fn layer() -> Layer<&'static str, Endpoint, ResHeader> {
    add_header::response::layer(L5D_REMOTE_IP, |endpoint: &Endpoint| {
        HeaderValue::from_shared(Bytes::from(endpoint.addr.ip().to_string())).ok()
    })
}
