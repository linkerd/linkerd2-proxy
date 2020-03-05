//! Adds `l5d-remote-ip` headers to http::Responses derived from the
//! `remote` of a `tls::accept::Meta`.

use super::HttpEndpoint;
use bytes::Bytes;
use http::header::HeaderValue;
use linkerd2_app_core::{
    proxy::http::add_header::{self, response::ResHeader, Layer},
    L5D_REMOTE_IP,
};

pub fn layer() -> Layer<&'static str, HttpEndpoint, ResHeader> {
    add_header::response::layer(L5D_REMOTE_IP, |endpoint: &HttpEndpoint| {
        HeaderValue::from_shared(Bytes::from(endpoint.addr.ip().to_string())).ok()
    })
}
