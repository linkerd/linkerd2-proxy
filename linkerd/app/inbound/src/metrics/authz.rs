use crate::policy::{AllowPolicy, Permit};
use linkerd_app_core::{
    metrics::{metrics, AuthzLabels, Counter, FmtMetrics, ServerLabel},
    transport::labels::TargetAddr,
};
use parking_lot::RwLock;
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
pub(crate) struct HttpAuthzMetrics(Arc<HttpInner>);

#[derive(Clone, Debug, Default)]
pub(crate) struct TcpAuthzMetrics(Arc<TcpInner>);

#[derive(Debug, Default)]
struct HttpInner {
    allow: RwLock<HashMap<(TargetAddr, AuthzLabels), Counter>>,
    deny: RwLock<HashMap<(TargetAddr, ServerLabel), Counter>>,
}

#[derive(Debug, Default)]
struct TcpInner {
    allow: RwLock<HashMap<(TargetAddr, AuthzLabels), Counter>>,
    deny: RwLock<HashMap<(TargetAddr, ServerLabel), Counter>>,
    terminate: RwLock<HashMap<(TargetAddr, ServerLabel), Counter>>,
}

fn server_labels(policy: &AllowPolicy) -> (TargetAddr, ServerLabel) {
    (TargetAddr(policy.dst_addr().into()), policy.server_label())
}

fn authz_labels(permit: &Permit) -> (TargetAddr, AuthzLabels) {
    (TargetAddr(permit.dst.into()), permit.labels.clone())
}

// === impl HttpAuthzMetrics ===

impl HttpAuthzMetrics {
    pub fn allow(&self, permit: &Permit) {
        self.0
            .allow
            .write()
            .entry(authz_labels(permit))
            .or_default()
            .incr();
    }

    pub fn deny(&self, policy: &AllowPolicy) {
        self.0
            .deny
            .write()
            .entry(server_labels(policy))
            .or_default()
            .incr();
    }
}

impl FmtMetrics for HttpAuthzMetrics {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let allow = self.0.allow.read();
        if !allow.is_empty() {
            inbound_http_authz_allow_total.fmt_help(f)?;
            inbound_http_authz_allow_total.fmt_scopes(f, allow.iter(), |c| c)?;
        }
        drop(allow);

        let deny = self.0.deny.read();
        if !deny.is_empty() {
            inbound_http_authz_deny_total.fmt_help(f)?;
            inbound_http_authz_deny_total.fmt_scopes(f, deny.iter(), |c| c)?;
        }
        drop(deny);

        Ok(())
    }
}

// === impl TcpAuthzMetrics ===

impl TcpAuthzMetrics {
    pub fn allow(&self, permit: &Permit) {
        self.0
            .allow
            .write()
            .entry(authz_labels(permit))
            .or_default()
            .incr();
    }

    pub fn deny(&self, policy: &AllowPolicy) {
        self.0
            .deny
            .write()
            .entry(server_labels(policy))
            .or_default()
            .incr();
    }

    pub fn terminate(&self, policy: &AllowPolicy) {
        self.0
            .terminate
            .write()
            .entry(server_labels(policy))
            .or_default()
            .incr();
    }
}

impl FmtMetrics for TcpAuthzMetrics {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let allow = self.0.allow.read();
        if !allow.is_empty() {
            inbound_tcp_authz_allow_total.fmt_help(f)?;
            inbound_tcp_authz_allow_total.fmt_scopes(f, allow.iter(), |c| c)?;
        }
        drop(allow);

        let deny = self.0.deny.read();
        if !deny.is_empty() {
            inbound_tcp_authz_deny_total.fmt_help(f)?;
            inbound_tcp_authz_deny_total.fmt_scopes(f, deny.iter(), |c| c)?;
        }
        drop(deny);

        let terminate = self.0.terminate.read();
        if !terminate.is_empty() {
            inbound_tcp_authz_terminate_total.fmt_help(f)?;
            inbound_tcp_authz_terminate_total.fmt_scopes(f, terminate.iter(), |c| c)?;
        }
        drop(terminate);

        Ok(())
    }
}
