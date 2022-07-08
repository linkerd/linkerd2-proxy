use http::{
    header::{HeaderName, HeaderValue},
    Request,
};

/// Adds or sets HTTP headers containing the client's IP address.
///
/// This is typically used to add headers such as
/// `Forwarded-For`, `X-Forwarded-For`, and friends.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct ClientIpHeaders {
    headers: Vec<(HeaderName, Action)>,
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum Action {
    Add,
    Set,
}

// === impl ForwardedFor ===

impl ClientIpHeaders {
    pub fn apply<B>(&self, req: &mut Request<B>) {
        if self.headers.is_empty() {
            return;
        }
        let value = match req.extensions().get::<linkerd_proxy_http::ClientHandle>() {
            Some(client) => HeaderValue::try_from(client.addr.ip().to_string())
                .expect("an IP address should format as a valid header value"),
            None => {
                debug_assert!(false, "request missing `ClientHandle` extension");
                return;
            }
        };

        let headers = req.headers_mut();
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
