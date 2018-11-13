use http;
use std::fmt;
use std::net::SocketAddr;

use super::classify;
use super::dst::DstAddr;
use proxy::http::{router, settings};
use proxy::server::Source;
use tap;
use transport::{connect, tls};
use {Conditional, NameAddr};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub addr: SocketAddr,
    pub dst_name: Option<NameAddr>,
    pub source_tls_status: tls::Status,
}

#[derive(Clone, Debug, Default)]
pub struct RecognizeEndpoint {
    default_addr: Option<SocketAddr>,
}

// === impl Endpoint ===

impl classify::CanClassify for Endpoint {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        classify::Request::default()
    }
}

impl Endpoint {
    pub fn dst_name(&self) -> Option<&NameAddr> {
        self.dst_name.as_ref()
    }

    fn target(&self) -> connect::Target {
        let tls = Conditional::None(tls::ReasonForNoTls::InternalTraffic);
        connect::Target::new(self.addr, tls)
    }
}

impl settings::router::HasConnect for Endpoint {
    fn connect(&self) -> connect::Target {
        self.target()
    }
}

impl From<Endpoint> for tap::Endpoint {
    fn from(ep: Endpoint) -> Self {
        tap::Endpoint {
            direction: tap::Direction::In,
            target: ep.target(),
            labels: Default::default(),
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.addr.fmt(f)
    }
}

// === impl RecognizeEndpoint ===

impl RecognizeEndpoint {
    pub fn new(default_addr: Option<SocketAddr>) -> Self {
        Self { default_addr }
    }
}

impl<A> router::Recognize<http::Request<A>> for RecognizeEndpoint {
    type Target = Endpoint;

    fn recognize(&self, req: &http::Request<A>) -> Option<Self::Target> {
        let src = req.extensions().get::<Source>();
        debug!("inbound endpoint: src={:?}", src);
        let addr = src
            .and_then(|s| s.orig_dst_if_not_local())
            .or(self.default_addr)?;

        let source_tls_status = src
            .map(|s| s.tls_status.clone())
            .unwrap_or_else(|| Conditional::None(tls::ReasonForNoTls::Disabled));

        let dst_name = req
            .extensions()
            .get::<DstAddr>()
            .and_then(|a| a.as_ref().name_addr())
            .cloned();
        debug!("inbound endpoint: dst={:?}", dst_name);

        Some(Endpoint {
            addr,
            dst_name,
            source_tls_status,
        })
    }
}

pub mod orig_proto_downgrade {
    use http;

    use proxy::http::orig_proto;
    use proxy::server::Source;
    use svc;

    #[derive(Debug, Clone)]
    pub struct Layer;

    #[derive(Clone, Debug)]
    pub struct Stack<M>
    where
        M: svc::Stack<Source>,
    {
        inner: M,
    }

    // === impl Layer ===

    pub fn layer() -> Layer {
        Layer
    }

    impl<M, A, B> svc::Layer<Source, Source, M> for Layer
    where
        M: svc::Stack<Source>,
        M::Value: svc::Service<Request = http::Request<A>, Response = http::Response<B>>,
    {
        type Value = <Stack<M> as svc::Stack<Source>>::Value;
        type Error = <Stack<M> as svc::Stack<Source>>::Error;
        type Stack = Stack<M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack { inner }
        }
    }

    // === impl Stack ===

    impl<M, A, B> svc::Stack<Source> for Stack<M>
    where
        M: svc::Stack<Source>,
        M::Value: svc::Service<Request = http::Request<A>, Response = http::Response<B>>,
    {
        type Value = orig_proto::Downgrade<M::Value>;
        type Error = M::Error;

        fn make(&self, target: &Source) -> Result<Self::Value, Self::Error> {
            info!("downgrading requests; source={:?}", target);
            let inner = self.inner.make(&target)?;
            Ok(inner.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use http;
    use std::net;

    use super::{Endpoint, RecognizeEndpoint};
    use proxy::http::router::Recognize;
    use proxy::server::Source;
    use transport::tls;
    use Conditional;

    fn make_h1_endpoint(addr: net::SocketAddr) -> Endpoint {
        let source_tls_status = TLS_DISABLED;
        Endpoint {
            addr,
            dst_name: None,
            source_tls_status,
        }
    }

    const TLS_DISABLED: Conditional<(), tls::ReasonForNoTls> =
        Conditional::None(tls::ReasonForNoTls::Disabled);

    quickcheck! {
        fn recognize_orig_dst(
            orig_dst: net::SocketAddr,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let src = Source::for_test(remote, local, Some(orig_dst), TLS_DISABLED);
            let rec = src.orig_dst_if_not_local().map(make_h1_endpoint);

            let mut req = http::Request::new(());
            req.extensions_mut().insert(src);

            RecognizeEndpoint::default().recognize(&req) == rec
        }

        fn recognize_default_no_orig_dst(
            default: Option<net::SocketAddr>,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let mut req = http::Request::new(());
            req.extensions_mut()
                .insert(Source::for_test(remote, local, None, TLS_DISABLED));

            RecognizeEndpoint::new(default).recognize(&req) == default.map(make_h1_endpoint)
        }

        fn recognize_default_no_ctx(default: Option<net::SocketAddr>) -> bool {
            let req = http::Request::new(());
            RecognizeEndpoint::new(default).recognize(&req) == default.map(make_h1_endpoint)
        }

        fn recognize_default_no_loop(
            default: Option<net::SocketAddr>,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let mut req = http::Request::new(());
            req.extensions_mut()
                .insert(Source::for_test(remote, local, Some(local), TLS_DISABLED));

            RecognizeEndpoint::new(default).recognize(&req) == default.map(make_h1_endpoint)
        }
    }
}
