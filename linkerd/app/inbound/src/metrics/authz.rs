use crate::policy::{AllowPolicy, Permit};
use linkerd_app_core::{
    metrics::{metrics, AuthzLabels, Counter, FmtMetrics, ServerLabel, TargetAddr, TlsAccept},
    tls,
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
    allow: Mutex<HashMap<((TargetAddr, AuthzLabels), tls::ConditionalServerTls), Counter>>,
    deny: Mutex<HashMap<((TargetAddr, ServerLabel), tls::ConditionalServerTls), Counter>>,
}

#[derive(Debug, Default)]
struct TcpInner {
    allow: Mutex<HashMap<((TargetAddr, AuthzLabels), tls::ConditionalServerTls), Counter>>,
    deny: Mutex<HashMap<((TargetAddr, ServerLabel), tls::ConditionalServerTls), Counter>>,
    terminate: Mutex<HashMap<((TargetAddr, ServerLabel), tls::ConditionalServerTls), Counter>>,
}

// === impl HttpAuthzMetrics ===

impl HttpAuthzMetrics {
    pub fn allow(&self, permit: &Permit, tls: tls::ConditionalServerTls) {
        let labels = ((TargetAddr(permit.dst.into()), permit.labels.clone()), tls);
        self.0.allow.lock().entry(labels).or_default().incr();
    }

    pub fn deny(&self, policy: &AllowPolicy, tls: tls::ConditionalServerTls) {
        let labels = (
            (TargetAddr(policy.dst_addr().into()), policy.server_label()),
            tls,
        );
        self.0.deny.lock().entry(labels).or_default().incr();
    }
}

impl FmtMetrics for HttpAuthzMetrics {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let allow = self.0.allow.lock();
        if !allow.is_empty() {
            inbound_http_authz_allow_total.fmt_help(f)?;
            inbound_http_authz_allow_total.fmt_scopes(
                f,
                allow.iter().map(|((l, tls), c)| ((l, TlsAccept(tls)), c)),
                |c| c,
            )?;
        }
        drop(allow);

        let deny = self.0.deny.lock();
        if !deny.is_empty() {
            inbound_http_authz_deny_total.fmt_help(f)?;
            inbound_http_authz_deny_total.fmt_scopes(
                f,
                deny.iter().map(|((l, tls), c)| ((l, TlsAccept(tls)), c)),
                |c| c,
            )?;
        }
        drop(deny);

        Ok(())
    }
}

// === impl TcpAuthzMetrics ===

impl TcpAuthzMetrics {
    pub fn allow(&self, permit: &Permit, tls: tls::ConditionalServerTls) {
        let labels = ((TargetAddr(permit.dst.into()), permit.labels.clone()), tls);
        self.0.allow.lock().entry(labels).or_default().incr();
    }

    pub fn deny(&self, policy: &AllowPolicy, tls: tls::ConditionalServerTls) {
        let labels = (
            (TargetAddr(policy.dst_addr().into()), policy.server_label()),
            tls,
        );
        self.0.deny.lock().entry(labels).or_default().incr();
    }

    pub fn terminate(&self, policy: &AllowPolicy, tls: tls::ConditionalServerTls) {
        let labels = (
            (TargetAddr(policy.dst_addr().into()), policy.server_label()),
            tls,
        );
        self.0.terminate.lock().entry(labels).or_default().incr();
    }
}

impl FmtMetrics for TcpAuthzMetrics {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let allow = self.0.allow.lock();
        if !allow.is_empty() {
            inbound_tcp_authz_allow_total.fmt_help(f)?;
            inbound_tcp_authz_allow_total.fmt_scopes(
                f,
                allow.iter().map(|((l, tls), c)| ((l, TlsAccept(tls)), c)),
                |c| c,
            )?;
        }
        drop(allow);

        let deny = self.0.deny.lock();
        if !deny.is_empty() {
            inbound_tcp_authz_deny_total.fmt_help(f)?;
            inbound_tcp_authz_deny_total.fmt_scopes(
                f,
                deny.iter().map(|((l, tls), c)| ((l, TlsAccept(tls)), c)),
                |c| c,
            )?;
        }
        drop(deny);

        let terminate = self.0.terminate.lock();
        if !terminate.is_empty() {
            inbound_tcp_authz_terminate_total.fmt_help(f)?;
            inbound_tcp_authz_terminate_total.fmt_scopes(
                f,
                terminate
                    .iter()
                    .map(|((l, tls), c)| ((l, TlsAccept(tls)), c)),
                |c| c,
            )?;
        }
        drop(terminate);

        Ok(())
    }
}
