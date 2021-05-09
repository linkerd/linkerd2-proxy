#![cfg(target_os = "disabled")]

use super::Endpoint;
use crate::{
    tcp,
    test_util::{
        support::{connect::Connect, http_util, profile, resolver, track},
        *,
    },
    Config, Outbound,
};
use bytes::Bytes;
use hyper::{client::conn::Builder as ClientBuilder, Body, Request, Response};
use linkerd_app_core::{
    io, profiles,
    svc::{self, NewService},
    tls,
    transport::{orig_dst, OrigDstAddr},
    Error, NameAddr, ProxyRuntime,
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

#[derive(Clone, Debug)]
struct NoTcpBalancer<T>(std::marker::PhantomData<fn(T)>);

fn no_tcp_balancer<T>() -> NoTcpBalancer<T> {
    NoTcpBalancer(std::marker::PhantomData)
}

impl<T: std::fmt::Debug> svc::NewService<T> for NoTcpBalancer<T> {
    type Service = Self;
    fn new_service(&mut self, target: T) -> Self::Service {
        panic!(
            "no TCP load balancer should be created in this test!\n\ttarget = {:?}",
            target
        );
    }
}

impl<T, I> svc::Service<I> for NoTcpBalancer<T> {
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        unreachable!("no TCP load balancer should be created in this test!");
    }

    fn call(&mut self, _: I) -> Self::Future {
        unreachable!("no TCP load balancer should be created in this test!");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unmeshed_http1_hello_world() {
    let mut server = hyper::server::conn::Http::new();
    server.http1_only(true);
    let client = ClientBuilder::new();
    unmeshed_hello_world(server, client).await;
}

#[tokio::test(flavor = "current_thread")]
async fn unmeshed_http2_hello_world() {
    let mut server = hyper::server::conn::Http::new();
    server.http2_only(true);
    let mut client = ClientBuilder::new();
    client.http2_only(true);
    unmeshed_hello_world(server, client).await;
}

#[tokio::test(flavor = "current_thread")]
async fn meshed_hello_world() {
    let _trace = support::trace_init();

    let ep1 = SocketAddr::new([10, 0, 0, 41].into(), 5550);
    let cfg = default_config();
    let id = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is invalid");
    let svc_addr = NameAddr::from_str("foo.ns1.svc.example.com:5550").unwrap();
    let meta = support::resolver::Metadata::new(
        Default::default(),
        support::resolver::ProtocolHint::Http2,
        None,
        Some(id.clone()),
        None,
    );

    // Pretend the upstream is a proxy that supports proto upgrades...
    let mut server_settings = hyper::server::conn::Http::new();
    server_settings.http2_only(true);
    let connect = support::connect().endpoint_fn_boxed(ep1, hello_server(server_settings));

    let profiles = profile::resolver().profile(
        ep1,
        profile::Profile {
            addr: Some(svc_addr.clone().into()),
            ..Default::default()
        },
    );

    let resolver = support::resolver::<support::resolver::Metadata>();
    let mut dst = resolver.endpoint_tx(svc_addr);
    dst.add(Some((ep1, meta.clone())))
        .expect("still listening to resolution");

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, resolver, connect).new_service(addrs(ep1));
    let (mut client, bg) = http_util::connect_and_accept(&mut ClientBuilder::new(), server).await;

    let rsp = http_util::http_request(&mut client, Request::default())
        .await
        .unwrap();
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await.unwrap();
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await.expect("background task failed");
}

async fn unmeshed_hello_world(
    server_settings: hyper::server::conn::Http,
    mut client_settings: ClientBuilder,
) {
    let _trace = support::trace_init();

    let ep1 = SocketAddr::new([10, 0, 0, 41].into(), 5550);
    let cfg = default_config();
    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn_boxed(ep1, hello_server(server_settings));

    let profiles = profile::resolver();
    let profile_tx = profiles.profile_tx(ep1);
    profile_tx.send(profile::Profile::default()).unwrap();

    let resolver = support::resolver::<support::resolver::Metadata>();

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, resolver, connect).new_service(addrs(ep1));
    let (mut client, bg) = http_util::connect_and_accept(&mut client_settings, server).await;

    let rsp = http_util::http_request(&mut client, Request::default())
        .await
        .unwrap();
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await.unwrap();
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await.expect("background task failed");
}

#[tracing::instrument]
fn hello_server(
    http: hyper::server::conn::Http,
) -> impl Fn(Endpoint) -> Result<io::BoxedIo, Error> {
    move |endpoint| {
        let span = tracing::info_span!("hello_server", ?endpoint);
        let _e = span.enter();
        tracing::info!("mock connecting");
        let (client_io, server_io) = support::io::duplex(4096);
        let hello_svc = hyper::service::service_fn(|request: Request<Body>| async move {
            tracing::info!(?request);
            Ok::<_, Error>(Response::new(Body::from("Hello world!")))
        });
        tokio::spawn(
            http.serve_connection(server_io, hello_svc)
                .in_current_span(),
        );
        Ok(io::BoxedIo::new(client_io))
    }
}
