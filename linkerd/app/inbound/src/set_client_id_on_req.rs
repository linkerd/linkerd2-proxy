//! Adds `l5d-client-id` headers to http::Requests derived from the
//! TlsIdentity of a `tls::accept::Meta`.

use http::header::HeaderValue;
use linkerd2_app_core::{
    proxy::http::add_header::{self, request::ReqHeader, Layer},
    transport::tls,
    Conditional, L5D_CLIENT_ID,
};
use tracing::{debug, warn};

pub fn layer() -> Layer<&'static str, tls::accept::Meta, ReqHeader> {
    add_header::request::layer(L5D_CLIENT_ID, |source: &tls::accept::Meta| {
        if let Conditional::Some(ref id) = source.peer_identity {
            if let Ok(value) = HeaderValue::from_str(id.as_ref()) {
                debug!("l5d-client-id enabled");
                return Some(value);
            }

            warn!("l5d-client-id identity header is invalid");
        }

        None
    })
}
