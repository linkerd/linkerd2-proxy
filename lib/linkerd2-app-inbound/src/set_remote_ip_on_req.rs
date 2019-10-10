//! Adds `l5d-remote-ip` headers to http::Requests derived from the
//! `remote` of a `Source`.

use bytes::Bytes;
use http::header::HeaderValue;
use linkerd2_app_core::L5D_REMOTE_IP;
use linkerd2_proxy_http::add_header::{self, request::ReqHeader, Layer};
use linkerd2_proxy_transport::Source;

pub fn layer() -> Layer<&'static str, Source, ReqHeader> {
    add_header::request::layer(L5D_REMOTE_IP, |source: &Source| {
        HeaderValue::from_shared(Bytes::from(source.remote.ip().to_string())).ok()
    })
}
