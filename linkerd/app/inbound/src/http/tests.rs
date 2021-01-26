#![allow(unused_imports)]
use super::HttpEndpoint;
use crate::test_util::{
    support::{connect::Connect, http_util, profile, resolver, track},
    *,
};
use crate::Config;
use crate::TcpEndpoint;
use hyper::{
    client::conn::{Builder as ClientBuilder, SendRequest},
    Body, Request, Response,
};
use linkerd_app_core::{
    drain,
    io::{self, BoxedIo},
    metrics,
    proxy::tap,
    svc::{self, NewService},
    tls,
    transport::{self, listen},
    Addr, Error, NameAddr,
};
use std::{
    net::SocketAddr,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
    time::Duration,
};
use tokio::time;
use tower::{Service, ServiceExt};
use tracing::Instrument;

fn build_http_server<I, T>(
    cfg: &Config,
    metrics: &metrics::Proxy,
    profiles: resolver::Profiles<NameAddr>,
    connect: Connect<TcpEndpoint>,
) -> (
    impl svc::NewService<
            (http::Version, T),
            Service = impl tower::Service<
                I,
                Response = (),
                Error = impl Into<linkerd_app_core::Error>,
                Future = impl Send + 'static,
            > + Send
                          + Clone,
        > + Clone,
    drain::Signal,
)
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
    for<'t> &'t T: Into<SocketAddr>,
    T: Clone,
{
    let tap = tap::Registry::new();

    let (drain_tx, drain) = drain::channel();
    let router = super::router(cfg, connect, profiles, tap, metrics, None);
    let svc = super::server(&cfg.proxy, router, metrics, None, drain);
    (svc, drain_tx)
}
