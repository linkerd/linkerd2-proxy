//! Adds `l5d-remote-ip` headers to http::Requests derived from the
//! `remote` of a `Source`.

use bytes::Bytes;
use http::header::HeaderValue;
use linkerd2_app_core::{
    proxy::http::add_header::{self, request::ReqHeader, Layer},
    transport::tls,
    L5D_REMOTE_IP,
};

pub fn layer() -> Layer<&'static str, tls::accept::Meta, ReqHeader> {
    add_header::request::layer(L5D_REMOTE_IP, |source: &tls::accept::Meta| {
        HeaderValue::from_shared(Bytes::from(source.addrs.peer().ip().to_string())).ok()
    })
}
