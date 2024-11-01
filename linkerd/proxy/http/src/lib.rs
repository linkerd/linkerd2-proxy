#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use http::{header::AsHeaderName, uri::Authority};
use linkerd_error::Error;

pub mod balance;
pub mod client;
pub mod client_handle;
pub mod detect;
mod glue;
pub mod h1;
pub mod h2;
mod header_from_target;
pub mod normalize_uri;
pub mod orig_proto;
mod override_authority;
mod retain;
mod server;
pub mod stream_timeouts;
pub mod strip_header;
pub mod timeout;
pub mod upgrade;

pub use self::{
    balance::NewBalance,
    classify::{
        Classify, ClassifyEos, ClassifyResponse, NewClassifyGate, NewClassifyGateSet,
        NewInsertClassifyResponse,
    },
    client_handle::{ClientHandle, SetClientHandle},
    detect::DetectHttp,
    header_from_target::NewHeaderFromTarget,
    normalize_uri::{MarkAbsoluteForm, NewNormalizeUri},
    override_authority::{AuthorityOverride, NewOverrideAuthority},
    retain::Retain,
    server::{NewServeHttp, Params as ServerParams, ServeHttp},
    stream_timeouts::{EnforceTimeouts, StreamTimeouts},
    strip_header::StripHeader,
    timeout::{NewTimeout, ResponseTimeout, ResponseTimeoutError},
};
pub use http::{
    header::{self, HeaderMap, HeaderName, HeaderValue},
    uri, Method, Request, Response, StatusCode,
};
pub use hyper::body::HttpBody;
pub use linkerd_http_box::{BoxBody, BoxRequest, BoxResponse, EraseResponse};
pub use linkerd_http_classify as classify;
pub use linkerd_http_executor::TracingExecutor;
pub use linkerd_http_insert as insert;
pub use linkerd_http_version::{self as version, Version};

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

impl HasH2Reason for ::hyper::Error {
    fn h2_reason(&self) -> Option<h2::Reason> {
        (self as &(dyn std::error::Error + 'static)).h2_reason()
    }
}

/// Returns an Authority from the value of `header`.
pub fn authority_from_header<B, K>(req: &http::Request<B>, header: K) -> Option<Authority>
where
    K: AsHeaderName,
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
