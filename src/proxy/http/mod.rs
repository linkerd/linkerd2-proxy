use super::super::identity;
use crate::Error;
use http::header::AsHeaderName;
use http::uri::Authority;

pub mod add_header;
pub mod balance;
pub mod canonicalize;
pub mod client;
pub mod fallback;
pub(super) mod glue;
pub mod h1;
pub mod h2;
pub mod header_from_target;
pub mod insert;
pub mod metrics;
pub mod normalize_uri;
pub mod orig_proto;
pub mod profiles;
pub mod retry;
pub mod router;
pub mod settings;
pub mod strip_header;
pub mod timeout;
pub mod upgrade;

pub use self::client::Client;
pub use self::glue::{ClientUsedTls, HttpBody as Body, HyperServerSvc};
pub use self::settings::Settings;

pub trait HasH2Reason {
    fn h2_reason(&self) -> Option<::h2::Reason>;
}

impl<'a> HasH2Reason for &'a (dyn std::error::Error + 'static) {
    fn h2_reason(&self) -> Option<::h2::Reason> {
        let mut cause = Some(*self);

        while let Some(err) = cause {
            if let Some(err) = err.downcast_ref::<::h2::Error>() {
                return err.h2_reason();
            }

            cause = err.source();
        }

        None
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
