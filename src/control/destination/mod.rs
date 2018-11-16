//! A client for the controller's Destination service.
//!
//! This client is split into two primary components: A `Resolver`, that routers use to
//! initiate service discovery for a given name, and a `background::Process` that
//! satisfies these resolution requests. These components are separated by a channel so
//! that the thread responsible for proxying data need not also do this administrative
//! work of communicating with the control plane.
//!
//! The number of active resolutions is not currently bounded by this module. Instead, we
//! trust that callers of `Resolver` enforce such a constraint (for example, via
//! `linkerd2_proxy_router`'s LRU cache). Additionally, users of this module must ensure
//! they consume resolutions as they are sent so that the response channels don't grow
//! without bounds.
//!
//! Furthermore, there are not currently any bounds on the number of endpoints that may be
//! returned for a single resolution. It is expected that the Destination service enforce
//! some reasonable upper bounds.
//!
//! ## TODO
//!
//! - Given that the underlying gRPC client has some max number of concurrent streams, we
//!   actually do have an upper bound on concurrent resolutions. This needs to be made
//!   more explicit.
//! - We need some means to limit the number of endpoints that can be returned for a
//!   single resolution so that `control::Cache` is not effectively unbounded.

use futures::{
    future,
    sync::mpsc,
    Async,
    Future,
    Poll,
    Stream
};
use indexmap::IndexMap;
use std::fmt;
use std::sync::{Arc, Weak};
use tower_http::HttpService;
use tower_grpc::{Body, BoxBody};

use dns;
use transport::tls;
use proxy::resolve::{self, Resolve, Update};

pub mod background;

use app::config::Namespaces;
use self::background::Background;
use {Conditional, NameAddr};

/// A handle to request resolutions from the background discovery task.
#[derive(Clone)]
pub struct Resolver {
    request_tx: mpsc::UnboundedSender<ResolveRequest>,
}

/// Requests that resolution updaes for `authority` be sent on `responder`.
#[derive(Debug)]
struct ResolveRequest {
    authority: NameAddr,
    responder: Responder,
}

/// A handle through which response updates may be sent.
#[derive(Debug)]
struct Responder {
    /// Sends updates from the controller to a `Resolution`.
    update_tx: mpsc::UnboundedSender<Update<Metadata>>,

    /// Indicates whether the corresponding `Resolution` is still active.
    active: Weak<()>,
}

#[derive(Debug)]
pub struct Resolution {
    /// Receives updates from the controller.
    update_rx: mpsc::UnboundedReceiver<Update<Metadata>>,

    /// Allows `Responder` to detect when its `Resolution` has been lost.
    ///
    /// `Responder` holds a weak reference to this `Arc` and can determine when this
    /// reference has been dropped.
    _active: Arc<()>,
}

/// Metadata describing an endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Metadata {
    /// Arbitrary endpoint labels. Primarily used for telemetry.
    labels: IndexMap<String, String>,

    /// A hint from the controller about what protocol (HTTP1, HTTP2, etc) the
    /// destination understands.
    protocol_hint: ProtocolHint,

    /// How to verify TLS for the endpoint.
    tls_identity: Conditional<tls::Identity, tls::ReasonForNoIdentity>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolHint {
    /// We don't what the destination understands, so forward messages in the
    /// protocol we received them in.
    Unknown,
    /// The destination can receive HTTP2 messages.
    Http2,
}

/// Returns a `Resolver` and a background task future.
///
/// The `Resolver` is used by a listener to request resolutions, while
/// the background future is executed on the controller thread's executor
/// to drive the background task.
pub fn new<T>(
    mut client: Option<T>,
    dns_resolver: dns::Resolver,
    namespaces: Namespaces,
    suffixes: Vec<dns::Suffix>,
    concurrency_limit: usize,
) -> (Resolver, impl Future<Item = (), Error = ()>)
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
    T::Error: fmt::Debug,
{
    let (request_tx, rx) = mpsc::unbounded();
    let disco = Resolver { request_tx };
    let mut bg = Background::new(
        rx,
        dns_resolver,
        namespaces,
        suffixes,
        concurrency_limit,
    );
    let task = future::poll_fn(move || bg.poll_rpc(&mut client));
    (disco, task)
}

// ==== impl Resolver =====

impl Resolve<NameAddr> for Resolver {
    type Endpoint = Metadata;
    type Resolution = Resolution;

    /// Start watching for address changes for a certain authority.
    fn resolve(&self, authority: &NameAddr) -> Resolution {
        trace!("resolve; authority={:?}", authority);
        let (update_tx, update_rx) = mpsc::unbounded();
        let active = Arc::new(());
        let req = {
            let authority = authority.clone();
            ResolveRequest {
                authority,
                responder: Responder {
                    update_tx,
                    active: Arc::downgrade(&active),
                },
            }
        };
        self.request_tx
            .unbounded_send(req)
            .expect("unbounded can't fail");

        Resolution {
            update_rx,
            _active: active,
        }
    }
}

impl resolve::Resolution for Resolution {
    type Endpoint = Metadata;
    type Error = ();

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error> {
        let up = try_ready!(self.update_rx.poll())
            .expect("resolution stream must be infinite");
        Ok(Async::Ready(up))
    }
}

// ===== impl Responder =====

impl Responder {
    fn is_active(&self) -> bool {
        self.active.upgrade().is_some()
    }
}

// ===== impl Metadata =====

impl Metadata {
    pub fn none(tls: tls::ReasonForNoIdentity) -> Self {
        Self {
            labels: IndexMap::default(),
            protocol_hint: ProtocolHint::Unknown,
            tls_identity: Conditional::None(tls),
        }
    }

    pub fn new(
        labels: IndexMap<String, String>,
        protocol_hint: ProtocolHint,
        tls_identity: Conditional<tls::Identity, tls::ReasonForNoIdentity>
    ) -> Self {
        Self {
            labels,
            protocol_hint,
            tls_identity,
        }
    }

    /// Returns the endpoint's labels from the destination service, if it has them.
    pub fn labels(&self) -> &IndexMap<String, String> {
        &self.labels
    }

    pub fn protocol_hint(&self) -> ProtocolHint {
        self.protocol_hint
    }

    pub fn tls_identity(&self) -> Conditional<&tls::Identity, tls::ReasonForNoIdentity> {
        self.tls_identity.as_ref()
    }
}
