use std::time::Duration;

use bytes::Bytes;
use h2;
use http;
use tower_h2::{self, BoxBody};
use tower_reconnect::Reconnect;

use conditional::Conditional;
use control::{
    util::{AddOrigin, Backoff, LogErrors},
};
use dns;
use timeout::Timeout;
use transport::{tls, HostAndPort, LookupAddressAndConnect};
use watch_service::Rebind;

/// Type of the client service stack used to make destination requests.
pub(super) type ClientService = AddOrigin<Backoff<LogErrors<Reconnect<
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
    >>>>;

/// The state needed to bind a new controller client stack.
pub(super) struct BindClient {
    backoff_delay: Duration,
    identity: Conditional<tls::Identity, tls::ReasonForNoTls>,
    host_and_port: HostAndPort,
    dns_resolver: dns::Resolver,
    log_ctx: ::logging::Client<&'static str, HostAndPort>,
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

impl Rebind<tls::ConditionalClientConfig> for BindClient {
    type Service = ClientService;
    fn rebind(
        &mut self,
        client_cfg: &tls::ConditionalClientConfig,
    ) -> Self::Service {
        let conn_cfg = match (&self.identity, client_cfg) {
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

        let reconnect = Reconnect::new(h2_client);
        let log_errors = LogErrors::new(reconnect);
        let backoff = Backoff::new(log_errors, self.backoff_delay);
        // TODO: Use AddOrigin in tower-http
        AddOrigin::new(scheme, authority, backoff)
    }

}
