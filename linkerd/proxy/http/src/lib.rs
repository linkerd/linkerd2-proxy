#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use linkerd_error::Error;

pub mod balance;
pub mod classify;
pub mod client;
pub mod detect;
mod executor;
mod glue;
mod header_from_target;
pub mod insert;
mod override_authority;
mod retain;
pub mod server;
pub mod strip_header;
mod timeout;
pub mod upgrade;
pub mod version;

pub use self::{
    balance::NewBalance,
    classify::{
        Classify, ClassifyEos, ClassifyResponse, NewClassifyGate, NewClassifyGateSet,
        NewInsertClassifyResponse,
    },
    detect::DetectHttp,
    executor::TracingExecutor,
    header_from_target::NewHeaderFromTarget,
    override_authority::{AuthorityOverride, NewOverrideAuthority},
    retain::Retain,
    server::{ClientHandle, NewServeHttp, Params as ServerParams, ServeHttp},
    strip_header::StripHeader,
    timeout::{NewTimeout, ResponseTimeout, ResponseTimeoutError},
    version::Version,
};
pub use http::{
    header::{HeaderName, HeaderValue},
    uri, Method, Request, Response, StatusCode,
};
pub use hyper::body::HttpBody;
pub use linkerd_http_box::{BoxBody, BoxRequest, BoxResponse, EraseResponse};

pub mod header {
    pub use ::http::header::*;

    pub const L5D_ORIG_PROTO: &str = "l5d-orig-proto";
}

#[derive(Clone, Debug)]
pub struct HeaderPair(pub HeaderName, pub HeaderValue);

pub trait HasH2Reason {
    fn h2_reason(&self) -> Option<::h2::Reason>;
}

impl<'a> HasH2Reason for &'a (dyn std::error::Error + 'static) {
    fn h2_reason(&self) -> Option<::h2::Reason> {
        if let Some(err) = self.downcast_ref::<::h2::Error>() {
            return err.h2_reason();
        }

        self.source().and_then(|e| e.h2_reason())
    }
}

impl HasH2Reason for Error {
    fn h2_reason(&self) -> Option<::h2::Reason> {
        (&**self as &(dyn std::error::Error + 'static)).h2_reason()
    }
}

impl HasH2Reason for ::h2::Error {
    fn h2_reason(&self) -> Option<::h2::Reason> {
        self.reason()
    }
}

/// Returns an Authority from the value of `header`.
pub fn authority_from_header<B, K>(req: &http::Request<B>, header: K) -> Option<uri::Authority>
where
    K: header::AsHeaderName,
{
    let v = req.headers().get(header)?;
    v.to_str().ok()?.parse().ok()
}

fn set_authority(uri: &mut uri::Uri, auth: uri::Authority) {
    let mut parts = uri::Parts::from(std::mem::take(uri));

    parts.authority = Some(auth);

    // If this was an origin-form target (path only),
    // then we can't *only* set the authority, as that's
    // an illegal target (such as `example.com/docs`).
    //
    // But don't set a scheme if this was authority-form (CONNECT),
    // since that would change its meaning (like `https://example.com`).
    if parts.path_and_query.is_some() {
        parts.scheme = Some(http::uri::Scheme::HTTP);
    }

    let new = http::uri::Uri::from_parts(parts).expect("absolute uri");

    *uri = new;
}

fn strip_connection_headers(headers: &mut http::HeaderMap) {
    if let Some(val) = headers.remove(header::CONNECTION) {
        if let Ok(conn_header) = val.to_str() {
            // A `Connection` header may have a comma-separated list of
            // names of other headers that are meant for only this specific
            // connection.
            //
            // Iterate these names and remove them as headers.
            for name in conn_header.split(',') {
                let name = name.trim();
                headers.remove(name);
            }
        }
    }

    // Additionally, strip these "connection-level" headers always, since
    // they are otherwise illegal if upgraded to HTTP2.
    headers.remove(header::UPGRADE);
    headers.remove("proxy-connection");
    headers.remove("keep-alive");
}
