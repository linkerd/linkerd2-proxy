use crate::policy::{AllowPolicy, HttpRoutePermit, ServerPermit};
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
}

#[derive(Debug, Default)]
struct TcpInner {
    allow: Mutex<HashMap<ServerAuthzKey, Counter>>,
    deny: Mutex<HashMap<ServerKey, Counter>>,
    terminate: Mutex<HashMap<ServerKey, Counter>>,
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

    pub fn deny(&self, labels: RouteLabels, dst: OrigDstAddr, tls: tls::ConditionalServerTls) {
        self.0
            .deny
            .lock()
            .entry(RouteKey::new(labels, dst, tls))
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
