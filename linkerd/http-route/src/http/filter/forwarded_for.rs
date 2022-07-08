use http::{
    header::{HeaderMap, HeaderName, HeaderValue},
};
use std::net::SocketAddr;

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct ForwardedFor {
    headers: Vec<Header>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Header {
    pub name: HeaderName,
    pub overwrite: bool,
}

// === impl ForwardedFor ===

impl ForwardedFor {
    pub fn apply(&self, client_addr: SocketAddr, headers: &mut HeaderMap) {
        if self.headers.is_empty() {
            return;
        }

        let value = HeaderValue::try_from(client_addr.to_string()).expect("a SocketAddr should be a valid header value");
        for Header { name, overwrite } in &self.headers {
            if *overwrite {
                headers.insert(name.clone(), value.clone());
            } else {
                headers.append(name.clone(), value.clone());
            }
        }
    }
}
