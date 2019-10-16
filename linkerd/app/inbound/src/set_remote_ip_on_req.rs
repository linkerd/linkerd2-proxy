//! Adds `l5d-remote-ip` headers to http::Requests derived from the
//! `remote` of a `Source`.

use bytes::Bytes;
use http::header::HeaderValue;
use linkerd2_app_core::{
    proxy::http::add_header::{self, request::ReqHeader, Layer},
    transport::Source,
    L5D_REMOTE_IP,
};

pub fn layer() -> Layer<&'static str, Source, ReqHeader> {
    add_header::request::layer(L5D_REMOTE_IP, |source: &Source| {
        HeaderValue::from_shared(Bytes::from(source.remote.ip().to_string())).ok()
    })
}
