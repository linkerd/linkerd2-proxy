use futures::{Async, Poll};
use futures_watch::Watch;
use http;
use std::fmt;
use std::marker::PhantomData;
use std::net::SocketAddr;

use app::{Destination, NameOrAddr};
use control::destination::{Metadata, ProtocolHint};
use proxy::{
    http::{client, orig_proto, router, Settings},
    resolve,
};
use svc;
use tap;
use transport::{connect, tls, DnsNameAndPort};
use Conditional;

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub dst: Destination,
    pub connect: connect::Target,
    pub metadata: Metadata,
    _p: (),
}

#[derive(Clone, Debug, Default)]
pub struct Recognize {}

#[derive(Clone, Debug)]
pub struct Resolve<R: resolve::Resolve<DnsNameAndPort>>(R);

#[derive(Debug)]
pub enum Resolution<R: resolve::Resolution> {
    Name(Destination, R),
    Addr(Destination, Option<SocketAddr>),
}

pub fn orig_proto_upgrade<M>() -> LayerUpgrade<M> {
    LayerUpgrade(PhantomData)
}

#[derive(Debug)]
pub struct LayerUpgrade<M>(PhantomData<fn() -> (M)>);

#[derive(Clone, Debug)]
pub struct StackUpgrade<M>
where
    M: svc::Stack<Endpoint>,
{
    inner: M,
}

#[derive(Debug)]
pub struct LayerTlsConfig<M: svc::Stack<Endpoint>> {
    watch: Watch<tls::ConditionalClientConfig>,
    _p: PhantomData<fn() -> (M)>,
}

#[derive(Clone, Debug)]
pub struct StackTlsConfig<M: svc::Stack<Endpoint>> {
    watch: Watch<tls::ConditionalClientConfig>,
    inner: M,
}

#[derive(Clone, Debug)]
pub struct StackEndpointWithTls<M: svc::Stack<Endpoint>> {
    endpoint: Endpoint,
    inner: M,
}

impl<B> router::Recognize<http::Request<B>> for Recognize {
    type Target = Destination;

    fn recognize(&self, req: &http::Request<B>) -> Option<Self::Target> {
        let dst = Destination::from_request(req);
        debug!("recognize: dst={:?}", dst);
        dst
    }
}

impl Recognize {
    pub fn new() -> Self {
        Self {}
    }
}

impl<R> Resolve<R>
where
    R: resolve::Resolve<DnsNameAndPort, Endpoint = Metadata>,
{
    pub fn new(resolve: R) -> Self {
        Resolve(resolve)
    }
}

impl<R> resolve::Resolve<Destination> for Resolve<R>
where
    R: resolve::Resolve<DnsNameAndPort, Endpoint = Metadata>,
{
    type Endpoint = Endpoint;
    type Resolution = Resolution<R::Resolution>;

    fn resolve(&self, dst: &Destination) -> Self::Resolution {
        match dst.name_or_addr {
            NameOrAddr::Name(ref name) => Resolution::Name(dst.clone(), self.0.resolve(&name)),
            NameOrAddr::Addr(ref addr) => Resolution::Addr(dst.clone(), Some(*addr)),
        }
    }
}

impl<R> resolve::Resolution for Resolution<R>
where
    R: resolve::Resolution<Endpoint = Metadata>,
{
    type Endpoint = Endpoint;
    type Error = R::Error;

    fn poll(&mut self) -> Poll<resolve::Update<Self::Endpoint>, Self::Error> {
        match self {
            Resolution::Name(ref dst, ref mut res) => match try_ready!(res.poll()) {
                resolve::Update::Remove(addr) => Ok(Async::Ready(resolve::Update::Remove(addr))),
                resolve::Update::Add(addr, metadata) => {
                    // If the endpoint does not have TLS, note the reason.
                    // Otherwise, indicate that we don't (yet) have a TLS
                    // config. This value may be changed by a stack layer that
                    // provides TLS configuration.
                    let tls = match metadata.tls_identity() {
                        Conditional::None(reason) => reason.into(),
                        Conditional::Some(_) => tls::ReasonForNoTls::NoConfig,
                    };
                    let ep = Endpoint {
                        dst: dst.clone(),
                        connect: connect::Target::new(addr, Conditional::None(tls)),
                        metadata,
                        _p: (),
                    };
                    Ok(Async::Ready(resolve::Update::Add(addr, ep)))
                }
            },
            Resolution::Addr(ref dst, ref mut addr) => match addr.take() {
                Some(addr) => {
                    let tls = tls::ReasonForNoIdentity::NoAuthorityInHttpRequest;
                    let ep = Endpoint {
                        dst: dst.clone(),
                        connect: connect::Target::new(addr, Conditional::None(tls.into())),
                        metadata: Metadata::none(tls),
                        _p: (),
                    };
                    let up = resolve::Update::Add(addr, ep);
                    Ok(Async::Ready(up))
                }
                None => Ok(Async::NotReady),
            },
        }
    }
}

impl<M> Clone for LayerUpgrade<M> {
    fn clone(&self) -> Self {
        LayerUpgrade(PhantomData)
    }
}

impl<M, A, B> svc::Layer<Endpoint, Endpoint, M> for LayerUpgrade<M>
where
    M: svc::Stack<Endpoint>,
    M::Value: svc::Service<Request = http::Request<A>, Response = http::Response<B>>,
{
    type Value = <StackUpgrade<M> as svc::Stack<Endpoint>>::Value;
    type Error = <StackUpgrade<M> as svc::Stack<Endpoint>>::Error;
    type Stack = StackUpgrade<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        StackUpgrade { inner }
    }
}

impl<M, A, B> svc::Stack<Endpoint> for StackUpgrade<M>
where
    M: svc::Stack<Endpoint>,
    M::Value: svc::Service<Request = http::Request<A>, Response = http::Response<B>>,
{
    type Value = svc::Either<orig_proto::Upgrade<M::Value>, M::Value>;
    type Error = M::Error;

    fn make(&self, endpoint: &Endpoint) -> Result<Self::Value, Self::Error> {
        if endpoint.can_use_orig_proto()
                && !endpoint.dst.settings.is_http2()
                && !endpoint.dst.settings.is_h1_upgrade() {
            let mut upgraded = endpoint.clone();
            upgraded.dst.settings = Settings::Http2;
            self.inner.make(&upgraded).map(|i| svc::Either::A(i.into()))
        } else {
            self.inner.make(&endpoint).map(svc::Either::B)
        }
    }
}

impl Endpoint {
pub fn can_use_orig_proto(&self) -> bool {
    match self.metadata.protocol_hint() {
        ProtocolHint::Unknown => false,
        ProtocolHint::Http2 => true,
    }
}
}

impl fmt::Display for Endpoint {
fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    self.connect.addr.fmt(f)
    }
}

// Stacks it possible to build a client::Stack<Endpoint>.
impl From<Endpoint> for client::Config {
    fn from(ep: Endpoint) -> Self {
        client::Config::new(ep.connect, ep.dst.settings)
    }
}

impl From<Endpoint> for tap::Endpoint {
    fn from(ep: Endpoint) -> Self {
        tap::Endpoint {
            direction: tap::Direction::Out,
            labels: ep.metadata.labels().clone(),
            client: ep.into(),
        }
    }
}

impl<M> LayerTlsConfig<M>
where
    M: svc::Stack<Endpoint> + Clone,
{
    pub fn new(watch: Watch<tls::ConditionalClientConfig>) -> Self {
        LayerTlsConfig {
            watch,
            _p: PhantomData,
        }
    }
}

impl<M> Clone for LayerTlsConfig<M>
where
    M: svc::Stack<Endpoint> + Clone,
{
    fn clone(&self) -> Self {
        Self::new(self.watch.clone())
    }
}

impl<M> svc::Layer<Endpoint, Endpoint, M> for LayerTlsConfig<M>
where
    M: svc::Stack<Endpoint> + Clone,
{
    type Value = <StackTlsConfig<M> as svc::Stack<Endpoint>>::Value;
    type Error = <StackTlsConfig<M> as svc::Stack<Endpoint>>::Error;
    type Stack = StackTlsConfig<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        StackTlsConfig {
            inner,
            watch: self.watch.clone(),
        }
    }
}

impl<M> svc::Stack<Endpoint> for StackTlsConfig<M>
where
    M: svc::Stack<Endpoint> + Clone,
{
    type Value = svc::watch::Service<tls::ConditionalClientConfig, StackEndpointWithTls<M>>;
    type Error = M::Error;

    fn make(&self, endpoint: &Endpoint) -> Result<Self::Value, Self::Error> {
        let inner = StackEndpointWithTls {
            endpoint: endpoint.clone(),
            inner: self.inner.clone(),
        };
        svc::watch::Service::try(self.watch.clone(), inner)
    }
}

impl<M> svc::Stack<tls::ConditionalClientConfig> for StackEndpointWithTls<M>
where
    M: svc::Stack<Endpoint>,
{
    type Value = M::Value;
    type Error = M::Error;

    fn make(
        &self,
        client_config: &tls::ConditionalClientConfig,
    ) -> Result<Self::Value, Self::Error> {
        let mut endpoint = self.endpoint.clone();
        endpoint.connect.tls = endpoint.metadata.tls_identity().and_then(|identity| {
            client_config.as_ref().map(|config| tls::ConnectionConfig {
                server_identity: identity.clone(),
                config: config.clone(),
            })
        });

        self.inner.make(&endpoint)
    }
}
