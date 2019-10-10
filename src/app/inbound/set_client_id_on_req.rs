//! Adds `l5d-client-id` headers to http::Requests derived from the
//! TlsIdentity of a `Source`.

use super::super::L5D_CLIENT_ID;
use crate::proxy::http::add_header::{self, request::ReqHeader, Layer};
use crate::transport::Source;
use crate::Conditional;
use http::header::HeaderValue;
use tracing::{debug, warn};

pub fn layer() -> Layer<&'static str, Source, ReqHeader> {
    add_header::request::layer(L5D_CLIENT_ID, |source: &Source| {
        if let Conditional::Some(ref id) = source.tls_peer {
            match HeaderValue::from_str(id.as_ref()) {
                Ok(value) => {
                    debug!("l5d-client-id enabled for {:?}", source);
                    return Some(value);
                }
                Err(_err) => {
                    warn!("l5d-client-id identity header is invalid: {:?}", source);
                }
            };
        }

        None
    })
}
