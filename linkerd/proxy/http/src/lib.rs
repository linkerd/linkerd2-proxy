#![deny(warnings, rust_2018_idioms)]
use http::header::AsHeaderName;
use http::uri::Authority;
use linkerd_error::Error;

pub mod balance;
pub mod client;
pub mod client_handle;
pub mod detect;
mod glue;
pub mod h1;
pub mod h2;
mod header_from_target;
pub mod insert;
pub mod normalize_uri;
pub mod orig_proto;
mod override_authority;
mod retain;
mod server;
mod set_identity_header;
pub mod strip_header;
pub mod timeout;
pub mod trace;
pub mod upgrade;
mod version;

pub use self::{
    client_handle::{ClientHandle, SetClientHandle},
    detect::DetectHttp,
    glue::{HyperServerSvc, UpgradeBody},
    header_from_target::NewHeaderFromTarget,
    normalize_uri::{MarkAbsoluteForm, NewNormalizeUri},
    override_authority::{AuthorityOverride, NewOverrideAuthority},
    retain::Retain,
    server::NewServeHttp,
    set_identity_header::NewSetIdentityHeader,
    timeout::MakeTimeoutLayer,
    version::Version,
};
pub use http::{
    header::{self, HeaderName, HeaderValue},
    uri, Request, Response, StatusCode,
};
pub use hyper::body::HttpBody;
pub use linkerd_http_box::{BoxBody, BoxRequest, BoxResponse};

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
pub fn authority_from_header<B, K>(req: &http::Request<B>, header: K) -> Option<Authority>
where
    K: AsHeaderName,
{
    let v = req.headers().get(header)?;
    v.to_str().ok()?.parse().ok()
}
