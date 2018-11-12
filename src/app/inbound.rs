use http;
use std::fmt;
use std::net::SocketAddr;

use super::classify;
use proxy::http::{client, normalize_uri::ShouldNormalizeUri, router, Settings};
use proxy::server::Source;
use svc::stack_per_request::ShouldStackPerRequest;
use tap;
use transport::{connect, tls};
use {Conditional, NameAddr};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub addr: SocketAddr,
    pub dst_name: Option<NameAddr>,
    pub settings: Settings,
    pub source_tls_status: tls::Status,
}

// === Recognize ===

#[derive(Clone, Debug, Default)]
pub struct Recognize {
    default_addr: Option<SocketAddr>,
}

// === impl Endpoint ===

impl ShouldNormalizeUri for Endpoint {
    fn should_normalize_uri(&self) -> bool {
        !self.settings.is_http2() && !self.settings.was_absolute_form()
    }
}

impl ShouldStackPerRequest for Endpoint {
    fn should_stack_per_request(&self) -> bool {
        !self.settings.is_http2() && !self.settings.can_reuse_clients()
    }
}

impl classify::CanClassify for Endpoint {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        classify::Request::default()
    }
}

// Makes it possible to build a client::Stack<Endpoint>.
impl From<Endpoint> for client::Config {
    fn from(ep: Endpoint) -> Self {
        let tls = Conditional::None(tls::ReasonForNoTls::InternalTraffic);
        let connect = connect::Target::new(ep.addr, tls);
        client::Config::new(connect, ep.settings)
    }
}

impl From<Endpoint> for tap::Endpoint {
    fn from(ep: Endpoint) -> Self {
        tap::Endpoint {
            direction: tap::Direction::In,
            client: ep.into(),
            labels: Default::default(),
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.addr.fmt(f)
    }
}

impl Recognize {
    pub fn new(default_addr: Option<SocketAddr>) -> Self {
        Self { default_addr }
    }
}

impl<A> router::Recognize<http::Request<A>> for Recognize {
    type Target = Endpoint;

    fn recognize(&self, req: &http::Request<A>) -> Option<Self::Target> {
        let src = req.extensions().get::<Source>();
        let source_tls_status = src
            .map(|s| s.tls_status.clone())
            .unwrap_or_else(|| Conditional::None(tls::ReasonForNoTls::Disabled));

        let addr = src
            .and_then(Source::orig_dst_if_not_local)
            .or(self.default_addr)?;

        let dst_name = super::http_request_addr(req)
            .ok()
            .and_then(|h| h.into_name_addr());
        let settings = Settings::from_request(req);

        let ep = Endpoint {
            addr,
            dst_name,
            settings,
            source_tls_status,
        };
        debug!("recognize: src={:?} ep={:?}", src, ep);
        Some(ep)
    }
}

impl fmt::Display for Recognize {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "in")
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

/// Rewrites connect `Target`s IP address to the loopback address (`127.0.0.1`),
/// with the same port still set.
pub mod rewrite_loopback_addr {
    use std::net::SocketAddr;
    use svc;
    use transport::connect::Target;

    #[derive(Debug, Clone)]
    pub struct Layer;

    #[derive(Clone, Debug)]
    pub struct Stack<M>
    where
        M: svc::Stack<Target>,
    {
        inner: M,
    }

    // === impl Layer ===

    pub fn layer() -> Layer {
        Layer
    }

    impl<M> svc::Layer<Target, Target, M> for Layer
    where
        M: svc::Stack<Target>,
    {
        type Value = <Stack<M> as svc::Stack<Target>>::Value;
        type Error = <Stack<M> as svc::Stack<Target>>::Error;
        type Stack = Stack<M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack { inner }
        }
    }

    // === impl Stack ===

    impl<M> svc::Stack<Target> for Stack<M>
    where
        M: svc::Stack<Target>,
    {
        type Value = M::Value;
        type Error = M::Error;

        fn make(&self, target: &Target) -> Result<Self::Value, Self::Error> {
            debug!("rewriting inbound address to loopback; target={:?}", target);

            let rewritten = SocketAddr::from(([127, 0, 0, 1], target.addr.port()));
            let target = Target::new(rewritten, target.tls.clone());
            self.inner.make(&target)
        }
    }
}

#[cfg(test)]
mod tests {
    use http;
    use std::net;

    use super::{Endpoint, Recognize};
    use proxy::http::router::Recognize as _Recognize;
    use proxy::http::settings::Settings;
    use proxy::server::Source;
    use transport::tls;
    use Conditional;

    fn make_h1_endpoint(addr: net::SocketAddr) -> Endpoint {
        let settings = Settings::Http1 {
            is_h1_upgrade: false,
            was_absolute_form: false,
            stack_per_request: true,
        };
        let source_tls_status = TLS_DISABLED;
        Endpoint {
            addr,
            dst_name: None,
            settings,
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

            Recognize::default().recognize(&req) == rec
        }

        fn recognize_default_no_orig_dst(
            default: Option<net::SocketAddr>,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let mut req = http::Request::new(());
            req.extensions_mut()
                .insert(Source::for_test(remote, local, None, TLS_DISABLED));

            Recognize::new(default).recognize(&req) == default.map(make_h1_endpoint)
        }

        fn recognize_default_no_ctx(default: Option<net::SocketAddr>) -> bool {
            let req = http::Request::new(());
            Recognize::new(default).recognize(&req) == default.map(make_h1_endpoint)
        }

        fn recognize_default_no_loop(
            default: Option<net::SocketAddr>,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let mut req = http::Request::new(());
            req.extensions_mut()
                .insert(Source::for_test(remote, local, Some(local), TLS_DISABLED));

            Recognize::new(default).recognize(&req) == default.map(make_h1_endpoint)
        }
    }
}
