#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use http::{header::AsHeaderName, uri::Authority};
use linkerd_error::Error;

pub mod balance;
pub mod client;
pub mod client_handle;
pub mod h1;
pub mod h2;
mod header_from_target;
pub mod normalize_uri;
pub mod orig_proto;
mod server;
pub mod strip_header;
pub mod timeout;

pub use self::{
    balance::NewBalance,
    classify::{
        Classify, ClassifyEos, ClassifyResponse, NewClassifyGate, NewClassifyGateSet,
        NewInsertClassifyResponse,
    },
    client_handle::{ClientHandle, SetClientHandle},
    header_from_target::NewHeaderFromTarget,
    normalize_uri::{MarkAbsoluteForm, NewNormalizeUri},
    server::{NewServeHttp, Params as ServerParams, ServeHttp},
    strip_header::StripHeader,
    timeout::{NewTimeout, ResponseTimeout, ResponseTimeoutError},
};
pub use http::{
    header::{self, HeaderMap, HeaderName, HeaderValue},
    uri, Method, Request, Response, StatusCode,
};
pub use http_body::Body;
pub use hyper_util::rt::tokio::TokioExecutor;
pub use linkerd_http_box::{BoxBody, BoxRequest, BoxResponse, EraseResponse};
pub use linkerd_http_classify as classify;
pub use linkerd_http_detect::{
    DetectMetrics, DetectMetricsFamilies, DetectParams, Detection, NewDetect,
};
pub use linkerd_http_insert as insert;
pub use linkerd_http_override_authority::{AuthorityOverride, NewOverrideAuthority};
pub use linkerd_http_retain::{self as retain, Retain};
pub use linkerd_http_stream_timeouts::{self as stream_timeouts, EnforceTimeouts, StreamTimeouts};
pub use linkerd_http_upgrade as upgrade;
pub use linkerd_http_variant::{Unsupported as UnsupportedVariant, Variant};

#[derive(Clone, Debug)]
pub struct HeaderPair(pub HeaderName, pub HeaderValue);

pub trait HasH2Reason {
    fn h2_reason(&self) -> Option<::h2::Reason>;
}

impl HasH2Reason for &(dyn std::error::Error + 'static) {
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
