pub mod http;
pub mod transport;

/// Indicates the orientation of traffic, relative to a sidecar proxy.
///
/// Each process exposes two proxies:
/// - The _inbound_ proxy receives traffic from another services forwards it to within the
///   local instance.
/// - The  _outbound_ proxy receives traffic from the local instance and forwards it to a
///   remote service.
///
/// This type is used for the purposes of caching and telemetry.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Proxy {
    Inbound,
    Outbound,
}

impl Proxy {
    pub fn as_str(&self) -> &'static str {
        match *self {
            Proxy::Inbound => "in",
            Proxy::Outbound => "out",
        }
    }
}

#[cfg(test)]
pub mod test_util {
    use http;
    use indexmap::IndexMap;
    use std::{
        net::SocketAddr,
        sync::Arc,
    };

    use ctx;
    use control::destination;
    use Conditional;
    use transport::tls;

    fn addr() -> SocketAddr {
        ([1, 2, 3, 4], 5678).into()
    }

    pub fn server(
        proxy: ctx::Proxy,
        tls: tls::Status
    ) -> Arc<ctx::transport::Server> {
        ctx::transport::Server::new(proxy, &addr(), &addr(), &Some(addr()), tls)
    }

    pub fn client(
        proxy: ctx::Proxy,
        labels: IndexMap<String, String>,
        tls: tls::Status,
    ) -> Arc<ctx::transport::Client> {
        let meta = destination::Metadata::new(
            labels,
            destination::ProtocolHint::Unknown,
            Conditional::None(tls::ReasonForNoIdentity::NotProvidedByServiceDiscovery)
        );
        ctx::transport::Client::new(proxy, &addr(), meta, tls)
    }

    pub fn request(
        uri: &str,
        server: &Arc<ctx::transport::Server>,
        client: &Arc<ctx::transport::Client>,
    ) -> (Arc<ctx::http::Request>, Arc<ctx::http::Response>) {
        let req = ctx::http::Request::new(
            &http::Request::get(uri).body(()).unwrap(),
            &server,
            &client,
        );
        let rsp = ctx::http::Response::new(
            &http::Response::builder().status(http::StatusCode::OK).body(()).unwrap(),
            &req,
        );
        (req, rsp)
    }
}
