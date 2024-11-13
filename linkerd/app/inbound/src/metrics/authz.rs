use crate::policy::{AllowPolicy, HttpRoutePermit, Meta, ServerPermit};
use linkerd_app_core::{
    metrics::{
        metrics, Counter, FmtLabels, FmtMetrics, RouteAuthzLabels, RouteLabels, ServerAuthzLabels,
        ServerLabel, TargetAddr, TlsAccept,
    },
    tls,
    transport::OrigDstAddr,
};
use parking_lot::Mutex;
use std::{collections::HashMap, sync::Arc};

metrics! {
    inbound_http_authz_allow_total: Counter {
        "The total number of inbound HTTP requests that were authorized"
    },
    inbound_http_authz_deny_total: Counter {
        "The total number of inbound HTTP requests that could not be processed due to a proxy error."
    },
    inbound_http_route_not_found_total: Counter {
        "The total number of inbound HTTP requests that could not be associated with a route"
    },

    inbound_http_local_ratelimit_total: Counter {
        "The total number of inbound HTTP requests that were rate-limited"
    },

    inbound_tcp_authz_allow_total: Counter {
        "The total number of inbound TCP connections that were authorized"
    },
    inbound_tcp_authz_deny_total: Counter {
        "The total number of inbound TCP connections that were denied"
    },
    inbound_tcp_authz_terminate_total: Counter {
        "The total number of inbound TCP connections that were terminated due to an authorization change"
    }
}

#[derive(Clone, Debug, Default)]
pub struct HttpAuthzMetrics(Arc<HttpInner>);

#[derive(Clone, Debug, Default)]
pub(crate) struct TcpAuthzMetrics(Arc<TcpInner>);

#[derive(Debug, Default)]
struct HttpInner {
    allow: Mutex<HashMap<RouteAuthzKey, Counter>>,
    deny: Mutex<HashMap<RouteKey, Counter>>,
    route_not_found: Mutex<HashMap<ServerKey, Counter>>,
    http_local_rate_limit: Mutex<HashMap<HttpLocalRateLimitKey, Counter>>,
}

#[derive(Debug, Default)]
struct TcpInner {
    allow: Mutex<HashMap<ServerAuthzKey, Counter>>,
    deny: Mutex<HashMap<ServerKey, Counter>>,
    terminate: Mutex<HashMap<ServerKey, Counter>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct HTTPLocalRateLimitLabels {
    pub server: ServerLabel,
    pub rate_limit: Option<Arc<Meta>>,
    pub scope: String,
}

#[derive(Debug, Hash, PartialEq, Eq)]
struct Key<L> {
    target: TargetAddr,
    tls: tls::ConditionalServerTls,
    labels: L,
}

type ServerKey = Key<ServerLabel>;
type ServerAuthzKey = Key<ServerAuthzLabels>;
type RouteKey = Key<RouteLabels>;
type RouteAuthzKey = Key<RouteAuthzLabels>;
type HttpLocalRateLimitKey = Key<HTTPLocalRateLimitLabels>;

// === impl HttpAuthzMetrics ===

impl HttpAuthzMetrics {
    pub fn allow(&self, permit: &HttpRoutePermit, tls: tls::ConditionalServerTls) {
        self.0
            .allow
            .lock()
            .entry(RouteAuthzKey::from_permit(permit, tls))
            .or_default()
            .incr();
    }

    pub fn route_not_found(
        &self,
        labels: ServerLabel,
        dst: OrigDstAddr,
        tls: tls::ConditionalServerTls,
    ) {
        self.0
            .route_not_found
            .lock()
            .entry(ServerKey::new(labels, dst, tls))
            .or_default()
            .incr();
    }

    pub fn deny(&self, labels: RouteLabels, dst: OrigDstAddr, tls: tls::ConditionalServerTls) {
        self.0
            .deny
            .lock()
            .entry(RouteKey::new(labels, dst, tls))
            .or_default()
            .incr();
    }

    pub fn ratelimit(
        &self,
        labels: HTTPLocalRateLimitLabels,
        dst: OrigDstAddr,
        tls: tls::ConditionalServerTls,
    ) {
        self.0
            .http_local_rate_limit
            .lock()
            .entry(HttpLocalRateLimitKey::new(labels, dst, tls))
            .or_default()
            .incr();
    }
}

impl FmtMetrics for HttpAuthzMetrics {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let allow = self.0.allow.lock();
        if !allow.is_empty() {
            inbound_http_authz_allow_total.fmt_help(f)?;
            inbound_http_authz_allow_total.fmt_scopes(
                f,
                allow
                    .iter()
                    .map(|(k, c)| ((k.target, (&k.labels, TlsAccept(&k.tls))), c)),
                |c| c,
            )?;
        }
        drop(allow);

        let deny = self.0.deny.lock();
        if !deny.is_empty() {
            inbound_http_authz_deny_total.fmt_help(f)?;
            inbound_http_authz_deny_total.fmt_scopes(
                f,
                deny.iter()
                    .map(|(k, c)| ((k.target, (&k.labels, TlsAccept(&k.tls))), c)),
                |c| c,
            )?;
        }
        drop(deny);

        let route_not_found = self.0.route_not_found.lock();
        if !route_not_found.is_empty() {
            inbound_http_route_not_found_total.fmt_help(f)?;
            inbound_http_route_not_found_total.fmt_scopes(
                f,
                route_not_found
                    .iter()
                    .map(|(k, c)| ((k.target, (&k.labels, TlsAccept(&k.tls))), c)),
                |c| c,
            )?;
        }
        drop(route_not_found);

        let local_ratelimit = self.0.http_local_rate_limit.lock();
        if !local_ratelimit.is_empty() {
            inbound_http_local_ratelimit_total.fmt_help(f)?;
            inbound_http_local_ratelimit_total.fmt_scopes(
                f,
                local_ratelimit
                    .iter()
                    .map(|(k, c)| ((k.target, (&k.labels, TlsAccept(&k.tls))), c)),
                |c| c,
            )?;
        }
        drop(local_ratelimit);

        Ok(())
    }
}

// === impl TcpAuthzMetrics ===

impl TcpAuthzMetrics {
    pub fn allow(&self, permit: &ServerPermit, tls: tls::ConditionalServerTls) {
        self.0
            .allow
            .lock()
            .entry(ServerAuthzKey::from_permit(permit, tls))
            .or_default()
            .incr();
    }

    pub fn deny(&self, policy: &AllowPolicy, tls: tls::ConditionalServerTls) {
        self.0
            .deny
            .lock()
            .entry(ServerKey::from_policy(policy, tls))
            .or_default()
            .incr();
    }

    pub fn terminate(&self, policy: &AllowPolicy, tls: tls::ConditionalServerTls) {
        self.0
            .terminate
            .lock()
            .entry(ServerKey::from_policy(policy, tls))
            .or_default()
            .incr();
    }
}

impl FmtMetrics for TcpAuthzMetrics {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let allow = self.0.allow.lock();
        if !allow.is_empty() {
            inbound_tcp_authz_allow_total.fmt_help(f)?;
            inbound_tcp_authz_allow_total.fmt_scopes(f, &*allow, |c| c)?;
        }
        drop(allow);

        let deny = self.0.deny.lock();
        if !deny.is_empty() {
            inbound_tcp_authz_deny_total.fmt_help(f)?;
            inbound_tcp_authz_deny_total.fmt_scopes(f, &*deny, |c| c)?;
        }
        drop(deny);

        let terminate = self.0.terminate.lock();
        if !terminate.is_empty() {
            inbound_tcp_authz_terminate_total.fmt_help(f)?;
            inbound_tcp_authz_terminate_total.fmt_scopes(f, &*terminate, |c| c)?;
        }
        drop(terminate);

        Ok(())
    }
}

// === impl HTTPLocalRateLimitLabels ===

impl FmtLabels for HTTPLocalRateLimitLabels {
    fn fmt_labels(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.server.fmt_labels(f)?;
        if let Some(rl) = &self.rate_limit {
            write!(
                f,
                ",ratelimit_group=\"{}\",ratelimit_kind=\"{}\",ratelimit_name=\"{}\",ratelimit_scope=\"{}\"",
                rl.group(),
                rl.kind(),
                rl.name(),
                self.scope,
            )
        } else {
            write!(f, ",ratelimit_scope=\"{}\"", self.scope)
        }
    }
}

// === impl Key ===

impl<L> Key<L> {
    fn new(labels: L, dst: OrigDstAddr, tls: tls::ConditionalServerTls) -> Self {
        Self {
            tls,
            target: TargetAddr(dst.into()),
            labels,
        }
    }
}

impl<L: FmtLabels> FmtLabels for Key<L> {
    fn fmt_labels(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (self.target, (&self.labels, TlsAccept(&self.tls))).fmt_labels(f)
    }
}

impl ServerKey {
    fn from_policy(policy: &AllowPolicy, tls: tls::ConditionalServerTls) -> Self {
        Self::new(policy.server_label(), policy.dst_addr(), tls)
    }
}

impl RouteAuthzKey {
    fn from_permit(permit: &HttpRoutePermit, tls: tls::ConditionalServerTls) -> Self {
        Self::new(permit.labels.clone(), permit.dst, tls)
    }
}

impl ServerAuthzKey {
    fn from_permit(permit: &ServerPermit, tls: tls::ConditionalServerTls) -> Self {
        Self::new(permit.labels.clone(), permit.dst, tls)
    }
}
