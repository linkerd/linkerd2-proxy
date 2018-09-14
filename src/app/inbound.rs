use http;
use std::fmt;
use std::marker::PhantomData;
use std::net::SocketAddr;

use proxy::http::{client, h1, orig_proto, router, Settings};
use proxy::server::Source;
use svc;
use tap;
use transport::{connect, tls};
use Conditional;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub addr: SocketAddr,
    pub authority: http::uri::Authority,
    pub settings: Settings,
    pub source_tls_status: tls::Status,
}

// === Recognize ===

#[derive(Clone, Debug, Default)]
pub struct Recognize {
    default_addr: Option<SocketAddr>,
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
            .and_then(|s| s.orig_dst_if_not_local())
            .or(self.default_addr)?;

        let authority = req.uri().authority_part().cloned()
            .or_else(|| h1::authority_from_host(req))
            .or_else(|| {
                let a = format!("{}", addr);
                http::uri::Authority::from_shared(a.into()).ok()
            })?;
        let settings = Settings::detect(req);

        let ep = Endpoint {
            addr,
            authority,
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

pub fn orig_proto_downgrade<M>() -> LayerDowngrade<M> {
    LayerDowngrade(PhantomData)
}

#[derive(Debug)]
pub struct LayerDowngrade<M>(PhantomData<fn() -> (M)>);

#[derive(Clone, Debug)]
pub struct StackDowngrade<M>
where
    M: svc::Stack<Source>,
{
    inner: M,
}

impl<M> Clone for LayerDowngrade<M> {
    fn clone(&self) -> Self {
        LayerDowngrade(PhantomData)
    }
}

impl<M, A, B> svc::Layer<Source, Source, M> for LayerDowngrade<M>
where
    M: svc::Stack<Source>,
    M::Value: svc::Service<Request = http::Request<A>, Response = http::Response<B>>,
{
    type Value = <StackDowngrade<M> as svc::Stack<Source>>::Value;
    type Error = <StackDowngrade<M> as svc::Stack<Source>>::Error;
    type Stack = StackDowngrade<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        StackDowngrade { inner }
    }
}

impl<M, A, B> svc::Stack<Source> for StackDowngrade<M>
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

// Stacks it possible to build a client::Stack<Endpoint>.
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

#[cfg(test)]
mod tests {
    use http;
    use std::net;

    use super::{Endpoint, Recognize};
    use proxy::http::router::Recognize as _Recognize;
    use proxy::http::settings::{Host, Settings};
    use proxy::server::Source;
    use transport::tls;
    use Conditional;

    fn make_h1_endpoint(addr: net::SocketAddr) -> Endpoint {
        let settings = Settings::Http1 {
            host: Host::NoAuthority,
            is_h1_upgrade: false,
            was_absolute_form: false,
        };
        let authority = http::uri::Authority::from_shared(format!("{}", addr).into()).unwrap();
        let source_tls_status = TLS_DISABLED;
        Endpoint {
            addr,
            authority,
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
