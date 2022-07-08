use http::header::{HeaderMap, HeaderName, HeaderValue};
use std::net::IpAddr;

/// Adds or sets HTTP headers containing the client's IP address.
///
/// This is typically used to add headers such as
/// `Forwarded-For`, `X-Forwarded-For`, and friends.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct ClientIpHeaders {
    pub headers: Vec<(HeaderName, Action)>,
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum Action {
    Add,
    Set,
}

// === impl ForwardedFor ===

impl ClientIpHeaders {
    pub fn apply(&self, client_addr: IpAddr, headers: &mut HeaderMap) {
        if self.headers.is_empty() {
            return;
        }

        let value = HeaderValue::try_from(client_addr.to_string())
            .expect("an IP address should format as a valid header value");
        for (header, action) in &self.headers {
            match action {
                Action::Add => {
                    headers.append(header.clone(), value.clone());
                }
                Action::Set => {
                    headers.insert(header.clone(), value.clone());
                }
            }
        }
    }
}
