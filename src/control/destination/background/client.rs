use std::time::Duration;

use bytes::Bytes;
use futures::Poll;
use h2;
use http;

use tower_h2::{self, BoxBody, RecvBody};
use tower_add_origin::AddOrigin;
use Conditional;
use dns;
use proxy;
use svc;
use timeout::Timeout;
use transport::{tls, HostAndPort, LookupAddressAndConnect};

/// Type of the client service stack used to make destination requests.
pub(super) struct ClientService(Service);

type Service = AddOrigin<
    proxy::reconnect::Service<
        HostAndPort,
        tower_h2::client::Connect<
            Timeout<LookupAddressAndConnect>,
            ::logging::ContextualExecutor<
                ::logging::Client<
                    &'static str,
                    HostAndPort
                >
            >,
            BoxBody,
        >
    >
>;

/// The state needed to bind a new controller client stack.
pub(super) struct BindClient {
    backoff_delay: Duration,
    identity: Conditional<tls::Identity, tls::ReasonForNoTls>,
    host_and_port: HostAndPort,
    dns_resolver: dns::Resolver,
    log_ctx: ::logging::Client<&'static str, HostAndPort>,
}

// ===== impl ClientService =====

impl svc::Service for ClientService {
    type Request = http::Request<BoxBody>;
    type Response = http::Response<RecvBody>;
    type Error = <Service as svc::Service>::Error;
    type Future = <Service as svc::Service>::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        // If an error occurs on established connection, log it; otherwise the `reconnect` module will handle re-establishing a client.
        loop {
            match self.0.poll_ready() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    info!("Controller client error: {}", e)
                }
            }
        }
    }

    fn call(&mut self, request: Self::Request) -> Self::Future {
        self.0.call(request)
    }
}

// ===== impl BindClient =====

impl BindClient {
    pub(super) fn new(
        identity: Conditional<tls::Identity, tls::ReasonForNoTls>,
        dns_resolver: &dns::Resolver,
        host_and_port: HostAndPort,
        backoff_delay: Duration,
    ) -> Self {
        let log_ctx = ::logging::admin().client("control", host_and_port.clone());
        Self {
            backoff_delay,
            identity,
            dns_resolver: dns_resolver.clone(),
            host_and_port,
            log_ctx,
        }
    }
}

impl svc::Stack<tls::ConditionalClientConfig> for BindClient {
    type Value = ClientService;
    type Error = ();

    fn make(&self, cfg: &tls::ConditionalClientConfig) -> Result<Self::Value, Self::Error> {
        let conn_cfg = match (&self.identity, cfg) {
            (Conditional::Some(ref id), Conditional::Some(ref cfg)) =>
                Conditional::Some(tls::ConnectionConfig {
                    server_identity: id.clone(),
                    config: cfg.clone(),
                }),
            (Conditional::None(ref reason), _) |
            (_, Conditional::None(ref reason)) =>
                Conditional::None(reason.clone()),
        };
        let scheme = http::uri::Scheme::from_shared(Bytes::from_static(b"http")).unwrap();
        let authority = http::uri::Authority::from(&self.host_and_port);
        let connect = Timeout::new(
            LookupAddressAndConnect::new(self.host_and_port.clone(),
                                         self.dns_resolver.clone(),
                                         conn_cfg),
            Duration::from_secs(3),
        );
        let h2_client = tower_h2::client::Connect::new(
            connect,
            h2::client::Builder::default(),
            self.log_ctx.clone().executor()
        );

        let reconnect = proxy::reconnect::Service::new(self.host_and_port.clone(), h2_client)
            .with_fixed_backoff(self.backoff_delay);
        Ok(ClientService(AddOrigin::new(reconnect, scheme, authority)))
    }

}
