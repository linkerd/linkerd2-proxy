//! Adds `l5d-remote-ip` headers to http::Requests derived from the
//! `remote` of a `Source`.

use super::super::L5D_REMOTE_IP;
use crate::proxy::{
    http::add_header::{self, request::ReqHeader, Layer},
    server::Source,
};
use bytes::Bytes;
use http::header::HeaderValue;

pub fn layer() -> Layer<&'static str, Source, ReqHeader> {
    add_header::request::layer(L5D_REMOTE_IP, |source: &Source| {
        HeaderValue::from_shared(Bytes::from(source.remote.ip().to_string())).ok()
    })
}
