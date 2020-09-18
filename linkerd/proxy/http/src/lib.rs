#![deny(warnings, rust_2018_idioms)]
use http::header::AsHeaderName;
use http::uri::Authority;
use linkerd2_error::Error;
use linkerd2_identity as identity;

pub mod add_header;
pub mod balance;
pub mod canonicalize;
pub mod client;
pub mod detect;
mod glue;
pub mod h1;
pub mod h2;
pub mod header_from_target;
pub mod insert;
pub mod normalize_uri;
pub mod orig_proto;
pub mod override_authority;
pub mod strip_header;
pub mod timeout;
pub mod trace;
pub mod upgrade;
mod version;

pub use self::{
    detect::DetectHttp,
    glue::{Body as Payload, HyperServerSvc},
    timeout::MakeTimeoutLayer,
    version::Version,
};
pub use http::{header, uri, Request, Response, StatusCode};
pub use hyper::body::HttpBody;
pub use linkerd2_http_box as boxed;

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
pub fn authority_from_header<B, K>(req: &http::Request<B>, header: K) -> Option<Authority>
where
    K: AsHeaderName,
{
    header_value_from_request(req, header, |s: &str| s.parse::<Authority>().ok())
}

pub fn identity_from_header<B, K>(req: &http::Request<B>, header: K) -> Option<identity::Name>
where
    K: AsHeaderName,
{
    header_value_from_request(req, header, |s: &str| {
        identity::Name::from_hostname(s.as_bytes()).ok()
    })
}

fn header_value_from_request<B, K, F, T>(
    req: &http::Request<B>,
    header: K,
    try_from: F,
) -> Option<T>
where
    K: AsHeaderName,
    F: FnOnce(&str) -> Option<T>,
{
    req.headers().get(header)?.to_str().ok().and_then(try_from)
}
