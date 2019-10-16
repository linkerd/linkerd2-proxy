//! Adds `l5d-server-id` headers to http::Responses derived from the
//! TlsIdentity of an `Endpoint`.

use super::Endpoint;
use http::header::HeaderValue;
use linkerd2_app_core::{
    proxy::http::add_header::{self, response::ResHeader, Layer},
    Conditional, L5D_SERVER_ID,
};
use tracing::{debug, warn};

pub fn layer() -> Layer<&'static str, Endpoint, ResHeader> {
    add_header::response::layer(L5D_SERVER_ID, |endpoint: &Endpoint| {
        if let Conditional::Some(id) = endpoint.identity.as_ref() {
            match HeaderValue::from_str(id.as_ref()) {
                Ok(value) => {
                    debug!("l5d-server-id enabled for {:?}", endpoint);
                    return Some(value);
                }
                Err(_err) => {
                    warn!("l5d-server-id identity header is invalid: {:?}", endpoint);
                }
            };
        }

        None
    })
}
