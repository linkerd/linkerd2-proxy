pub mod http;
pub mod transport;

/// Indicates the orientation of traffic, relative to a sidecar proxy.
///
/// Each process exposes two proxies:
/// - The _inbound_ proxy receives traffic from another services forwards it to within the
///   local instance.
/// - The  _outbound_ proxy receives traffic from the local instance and forwards it to a
///   remove service.
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
    use std::{
        fmt,
        net::SocketAddr,
        sync::Arc,
    };

    use ctx;
    use control::destination;
    use telemetry::DstLabels;
    use tls;
    use conditional::Conditional;

    fn addr() -> SocketAddr {
        ([1, 2, 3, 4], 5678).into()
    }

    pub fn server(
        proxy: ctx::Proxy,
        tls: ctx::transport::TlsStatus
    ) -> Arc<ctx::transport::Server> {
        ctx::transport::Server::new(proxy, &addr(), &addr(), &Some(addr()), tls)
    }

    pub fn client<L, S>(
        proxy: ctx::Proxy,
        labels: L,
        tls: ctx::transport::TlsStatus,
    ) -> Arc<ctx::transport::Client>
    where
        L: IntoIterator<Item=(S, S)>,
        S: fmt::Display,
    {
        let meta = destination::Metadata::new(DstLabels::new(labels),
            destination::ProtocolHint::Unknown,
            Conditional::None(tls::ReasonForNoIdentity::NotProvidedByServiceDiscovery));
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
