use crate::{
    policy,
    test_util::{
        support::{connect::Connect, http_util, profile, resolver},
        *,
    },
    Config, Inbound,
};
use hyper::{Request, Response};
use linkerd_app_core::{
    classify,
    errors::header::L5D_PROXY_ERROR,
    identity, io, metrics,
    proxy::http::{self, BoxBody},
    svc::{self, http::TracingExecutor, NewService, Param},
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote, ServerAddr},
    Error, NameAddr, ProxyRuntime,
};
use linkerd_app_test::connect::ConnectFuture;
use linkerd_tracing::test::trace_init;
use std::{net::SocketAddr, sync::Arc};
use tokio::time;
use tower::ServiceExt;
use tracing::Instrument;

fn build_server<I>(
    cfg: Config,
    rt: ProxyRuntime,
    profiles: resolver::Profiles,
    connect: Connect<Remote<ServerAddr>>,
) -> svc::ArcNewTcp<Target, I>
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
{
    Inbound::new(cfg, rt, &mut Default::default())
        .with_stack(connect)
        .map_stack(|cfg, _, s| {
            s.push_map_target(|t| Param::<Remote<ServerAddr>>::param(&t))
                .push_connect_timeout(cfg.proxy.connect.timeout)
        })
        .push_http_router(profiles)
        .push_http_server()
        .push_http_tcp_server()
        .into_inner()
}

#[tokio::test(flavor = "current_thread")]
async fn unmeshed_http1_hello_world() {
    let mut server = hyper::server::conn::http1::Builder::new();
    server.timer(hyper_util::rt::TokioTimer::new());
    let mut client = hyper::client::conn::http1::Builder::new();

    let _trace = trace_init();

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), hello_server(server));

    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();

    // Build the outbound server
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_HTTP1);
    let (mut client, bg) = http_util::connect_and_accept_http1(&mut client, server).await;

    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(BoxBody::default())
        .unwrap();
    let rsp = client
        .send_request(req)
        .await
        .expect("HTTP client request failed");
    tracing::info!(?rsp);
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await.unwrap();
    assert_eq!(body, "Hello world!");

    // Wait for all of the background tasks to complete, panicking if any returned an error.
    drop(client);
    bg.join_all()
        .await
        .into_iter()
        .collect::<Result<Vec<()>, Error>>()
        .expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn downgrade_origin_form() {
    // Reproduces https://github.com/linkerd/linkerd2/issues/5298
    let mut server = hyper::server::conn::http1::Builder::new();
    server.timer(hyper_util::rt::TokioTimer::new());
    let mut client = hyper::client::conn::http2::Builder::new(TracingExecutor);
    client.timer(hyper_util::rt::TokioTimer::new());
    let _trace = trace_init();

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), hello_server(server));

    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 80).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();

    // Build the outbound server
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_H2);
    let (mut client, bg) = {
        tracing::info!(settings = ?client, "connecting client with");
        let (client_io, server_io) = io::duplex(4096);

        let (client, conn) = client
            .handshake(hyper_util::rt::TokioIo::new(client_io))
            .await
            .expect("Client must connect");

        let mut bg = tokio::task::JoinSet::new();
        bg.spawn(
            async move {
                server.oneshot(server_io).await?;
                tracing::info!("proxy serve task complete");
                Ok(())
            }
            .instrument(tracing::info_span!("proxy")),
        );
        bg.spawn(
            async move {
                conn.await?;
                tracing::info!("client background complete");
                Ok(())
            }
            .instrument(tracing::info_span!("client_bg")),
        );

        (client, bg)
    };

    let req = Request::builder()
        .method(http::Method::GET)
        .uri("/")
        .header(http::header::HOST, "foo.svc.cluster.local")
        .header("l5d-orig-proto", "HTTP/1.1")
        .body(BoxBody::empty())
        .unwrap();
    let rsp = client
        .send_request(req)
        .await
        .expect("HTTP client request failed");
    tracing::info!(?rsp);
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await.unwrap();
    assert_eq!(body, "Hello world!");

    // Wait for all of the background tasks to complete, panicking if any returned an error.
    drop(client);
    bg.join_all()
        .await
        .into_iter()
        .collect::<Result<Vec<()>, Error>>()
        .expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn downgrade_absolute_form() {
    let mut client = hyper::client::conn::http2::Builder::new(TracingExecutor);
    client.timer(hyper_util::rt::TokioTimer::new());
    let mut server = hyper::server::conn::http1::Builder::new();
    server.timer(hyper_util::rt::TokioTimer::new());
    let _trace = trace_init();

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), hello_server(server));

    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 80).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();

    // Build the outbound server
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_H2);

    let (mut client, bg) = {
        tracing::info!(settings = ?client, "connecting client with");
        let (client_io, server_io) = io::duplex(4096);

        let (client, conn) = client
            .handshake(hyper_util::rt::TokioIo::new(client_io))
            .await
            .expect("Client must connect");

        let mut bg = tokio::task::JoinSet::new();
        bg.spawn(
            async move {
                server.oneshot(server_io).await?;
                tracing::info!("proxy serve task complete");
                Ok(())
            }
            .instrument(tracing::info_span!("proxy")),
        );
        bg.spawn(
            async move {
                conn.await?;
                tracing::info!("client background complete");
                Ok(())
            }
            .instrument(tracing::info_span!("client_bg")),
        );

        (client, bg)
    };

    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550/")
        .header(http::header::HOST, "foo.svc.cluster.local")
        .header("l5d-orig-proto", "HTTP/1.1; absolute-form")
        .body(BoxBody::empty())
        .unwrap();
    let rsp = client
        .send_request(req)
        .await
        .expect("HTTP client request failed");
    tracing::info!(?rsp);
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await.unwrap();
    assert_eq!(body, "Hello world!");

    // Wait for all of the background tasks to complete, panicking if any returned an error.
    drop(client);
    bg.join_all()
        .await
        .into_iter()
        .collect::<Result<Vec<()>, Error>>()
        .expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn http1_bad_gateway_meshed_response_error_header() {
    let _trace = trace_init();

    // Build a mock connect that always errors.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), connect_error());

    // Build a client using the connect that always errors so that responses
    // are BAD_GATEWAY.
    let mut client = hyper::client::conn::http1::Builder::new();
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::meshed_http1());
    let (mut client, bg) = http_util::connect_and_accept_http1(&mut client, server).await;

    // Send a request and assert that it is a BAD_GATEWAY with the expected
    // header message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(BoxBody::default())
        .unwrap();
    let rsp = client
        .send_request(req)
        .await
        .expect("HTTP client request failed");
    tracing::info!(?rsp);
    assert_eq!(rsp.status(), http::StatusCode::BAD_GATEWAY);
    // NOTE: this does not include a stack error context for that endpoint
    // because we don't build a real HTTP endpoint stack, which adds error
    // context to this error, and the client rescue layer is below where the
    // logical error context is added.
    check_error_header(rsp.headers(), "client error (Connect)");

    // Wait for all of the background tasks to complete, panicking if any returned an error.
    drop(client);
    bg.join_all()
        .await
        .into_iter()
        .collect::<Result<Vec<()>, Error>>()
        .expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn http1_bad_gateway_unmeshed_response() {
    let _trace = trace_init();

    // Build a mock connect that always errors.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), connect_error());

    // Build a client using the connect that always errors so that responses
    // are BAD_GATEWAY.
    let mut client = hyper::client::conn::http1::Builder::new();
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_HTTP1);
    let (mut client, bg) = http_util::connect_and_accept_http1(&mut client, server).await;

    // Send a request and assert that it is a BAD_GATEWAY with the expected
    // header message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(BoxBody::default())
        .unwrap();
    let rsp = client
        .send_request(req)
        .await
        .expect("HTTP client request failed");
    tracing::info!(?rsp);
    assert_eq!(rsp.status(), http::StatusCode::BAD_GATEWAY);
    assert!(
        rsp.headers().get(L5D_PROXY_ERROR).is_none(),
        "response must not contain L5D_PROXY_ERROR header"
    );

    // Wait for all of the background tasks to complete, panicking if any returned an error.
    drop(client);
    bg.join_all()
        .await
        .into_iter()
        .collect::<Result<Vec<()>, Error>>()
        .expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn http1_connect_timeout_meshed_response_error_header() {
    let _trace = trace_init();
    tokio::time::pause();

    // Build a mock connect that sleeps longer than the default inbound
    // connect timeout.
    let connect = support::connect().endpoint(Target::addr(), connect_timeout());

    // Build a client using the connect that always sleeps so that responses
    // are GATEWAY_TIMEOUT.
    let mut client = hyper::client::conn::http1::Builder::new();
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::meshed_http1());
    let (mut client, bg) = http_util::connect_and_accept_http1(&mut client, server).await;

    // Send a request and assert that it is a GATEWAY_TIMEOUT with the
    // expected header message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(BoxBody::default())
        .unwrap();
    let rsp = client
        .send_request(req)
        .await
        .expect("HTTP client request failed");
    tracing::info!(?rsp);
    assert_eq!(rsp.status(), http::StatusCode::GATEWAY_TIMEOUT);

    // NOTE: this does not include a stack error context for that endpoint
    // because we don't build a real HTTP endpoint stack, which adds error
    // context to this error, and the client rescue layer is below where the
    // logical error context is added.
    check_error_header(rsp.headers(), "client error (Connect)");

    // Wait for all of the background tasks to complete, panicking if any returned an error.
    drop(client);
    bg.join_all()
        .await
        .into_iter()
        .collect::<Result<Vec<()>, Error>>()
        .expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn http1_connect_timeout_unmeshed_response_error_header() {
    let _trace = trace_init();
    tokio::time::pause();

    // Build a mock connect that sleeps longer than the default inbound
    // connect timeout.
    let connect = support::connect().endpoint(Target::addr(), connect_timeout());

    // Build a client using the connect that always sleeps so that responses
    // are GATEWAY_TIMEOUT.
    let mut client = hyper::client::conn::http1::Builder::new();
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_HTTP1);
    let (mut client, bg) = http_util::connect_and_accept_http1(&mut client, server).await;

    // Send a request and assert that it is a GATEWAY_TIMEOUT with the
    // expected header message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(BoxBody::empty())
        .unwrap();
    let rsp = client
        .send_request(req)
        .await
        .expect("HTTP client request failed");
    tracing::info!(?rsp);
    assert_eq!(rsp.status(), http::StatusCode::GATEWAY_TIMEOUT);
    assert!(
        rsp.headers().get(L5D_PROXY_ERROR).is_none(),
        "response must not contain L5D_PROXY_ERROR header"
    );

    // Wait for all of the background tasks to complete, panicking if any returned an error.
    drop(client);
    bg.join_all()
        .await
        .into_iter()
        .collect::<Result<Vec<()>, Error>>()
        .expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn h2_response_meshed_error_header() {
    let _trace = trace_init();

    // Build a mock connect that always errors.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), connect_error());

    // Build a client using the connect that always errors.
    let mut client = hyper::client::conn::http2::Builder::new(TracingExecutor);
    client.timer(hyper_util::rt::TokioTimer::new());
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::meshed_h2());
    let (mut client, bg) = http_util::connect_and_accept_http2(&mut client, server).await;

    // Send a request and assert that it is SERVICE_UNAVAILABLE with the
    // expected header message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(BoxBody::empty())
        .unwrap();
    let rsp = client
        .send_request(req)
        .await
        .expect("HTTP client request failed");
    tracing::info!(?rsp);
    assert_eq!(rsp.status(), http::StatusCode::GATEWAY_TIMEOUT);

    check_error_header(rsp.headers(), "service in fail-fast");

    // Drop the client and discard the result of awaiting the proxy background
    // task. The result is discarded because it hits an error that is related
    // to the mock implementation and has no significance to the test.
    let _ = bg.join_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn h2_response_unmeshed_error_header() {
    let _trace = trace_init();

    // Build a mock connect that always errors.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), connect_error());

    // Build a client using the connect that always errors.
    let mut client = hyper::client::conn::http2::Builder::new(TracingExecutor);
    client.timer(hyper_util::rt::TokioTimer::new());
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_H2);
    let (mut client, bg) = http_util::connect_and_accept_http2(&mut client, server).await;

    // Send a request and assert that it is SERVICE_UNAVAILABLE with the
    // expected header message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(BoxBody::default())
        .unwrap();
    let rsp = client
        .send_request(req)
        .await
        .expect("HTTP client request failed");
    tracing::info!(?rsp);
    assert_eq!(rsp.status(), http::StatusCode::GATEWAY_TIMEOUT);
    assert!(
        rsp.headers().get(L5D_PROXY_ERROR).is_none(),
        "response must not contain L5D_PROXY_ERROR header"
    );

    // Drop the client and discard the result of awaiting the proxy background
    // task. The result is discarded because it hits an error that is related
    // to the mock implementation and has no significance to the test.
    let _ = bg.join_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_meshed_response_error_header() {
    let _trace = trace_init();

    // Build a mock connect that always errors.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), connect_error());

    // Build a client using the connect that always errors.
    let mut client = hyper::client::conn::http2::Builder::new(TracingExecutor);
    client.timer(hyper_util::rt::TokioTimer::new());
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::meshed_h2());
    let (mut client, bg) = http_util::connect_and_accept_http2(&mut client, server).await;

    // Send a request and assert that it is OK with the expected header
    // message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .header(http::header::CONTENT_TYPE, "application/grpc")
        .body(BoxBody::default())
        .unwrap();
    let rsp = client
        .send_request(req)
        .await
        .expect("HTTP client request failed");
    tracing::info!(?rsp);
    assert_eq!(rsp.status(), http::StatusCode::OK);

    check_error_header(rsp.headers(), "service in fail-fast");

    // Drop the client and discard the result of awaiting the proxy background
    // task. The result is discarded because it hits an error that is related
    // to the mock implementation and has no significance to the test.
    let _ = bg.join_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_unmeshed_response_error_header() {
    let _trace = trace_init();

    // Build a mock connect that always errors.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), connect_error());

    // Build a client using the connect that always errors.
    let mut client = hyper::client::conn::http2::Builder::new(TracingExecutor);
    client.timer(hyper_util::rt::TokioTimer::new());
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_H2);
    let (mut client, bg) = http_util::connect_and_accept_http2(&mut client, server).await;

    // Send a request and assert that it is OK with the expected header
    // message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .header(http::header::CONTENT_TYPE, "application/grpc")
        .body(BoxBody::default())
        .unwrap();
    let rsp = client
        .send_request(req)
        .await
        .expect("HTTP client request failed");
    tracing::info!(?rsp);
    assert_eq!(rsp.status(), http::StatusCode::OK);
    assert!(
        rsp.headers().get(L5D_PROXY_ERROR).is_none(),
        "response must not contain L5D_PROXY_ERROR header"
    );

    // Drop the client and discard the result of awaiting the proxy background
    // task. The result is discarded because it hits an error that is related
    // to the mock implementation and has no significance to the test.
    let _ = bg.join_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_response_class() {
    let _trace = trace_init();

    // Build a mock connector serves a gRPC server that returns errors.
    let connect = {
        let mut server = hyper::server::conn::http2::Builder::new(TracingExecutor);
        server.timer(hyper_util::rt::TokioTimer::new());
        support::connect().endpoint_fn_boxed(
            Target::addr(),
            grpc_status_server(server, tonic::Code::Unknown),
        )
    };

    // Build a client using the connect that always errors.
    let mut client = hyper::client::conn::http2::Builder::new(TracingExecutor);
    client.timer(hyper_util::rt::TokioTimer::new());
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let metrics = rt
        .metrics
        .clone()
        .http_endpoint
        .into_report(time::Duration::from_secs(3600));
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::meshed_h2());
    let (mut client, bg) = http_util::connect_and_accept_http2(&mut client, server).await;

    // Send a request and assert that it is OK with the expected header
    // message.
    let req = Request::builder()
        .method(http::Method::POST)
        .uri("http://foo.svc.cluster.local:5550")
        .header(http::header::CONTENT_TYPE, "application/grpc")
        .body(BoxBody::default())
        .unwrap();

    let rsp = client
        .send_request(req)
        .await
        .expect("HTTP client request failed");
    tracing::info!(?rsp);
    assert_eq!(rsp.status(), http::StatusCode::OK);

    use http_body_util::BodyExt;
    let mut body = rsp.into_body();
    let trls = body
        .frame()
        .await
        .unwrap()
        .unwrap()
        .into_trailers()
        .expect("trailers frame");
    assert_eq!(trls.get("grpc-status").unwrap().to_str().unwrap(), "2");

    let response_total = metrics
        .get_response_total(
            &metrics::EndpointLabels::Inbound(metrics::InboundEndpointLabels {
                tls: Target::meshed_h2().1,
                authority: Some("foo.svc.cluster.local:5550".parse().unwrap()),
                target_addr: "127.0.0.1:80".parse().unwrap(),
                policy: metrics::RouteAuthzLabels {
                    route: metrics::RouteLabels {
                        server: metrics::ServerLabel(
                            Arc::new(policy::Meta::Resource {
                                group: "policy.linkerd.io".into(),
                                kind: "server".into(),
                                name: "testsrv".into(),
                            }),
                            80,
                        ),
                        route: policy::Meta::new_default("default"),
                    },
                    authz: Arc::new(policy::Meta::Resource {
                        group: "policy.linkerd.io".into(),
                        kind: "serverauthorization".into(),
                        name: "testsaz".into(),
                    }),
                },
            }),
            Some(http::StatusCode::OK),
            &classify::Class::Grpc(Err(tonic::Code::Unknown)),
        )
        .expect("response_total not found");
    assert_eq!(response_total, 1.0);

    drop(bg);
}

#[tracing::instrument]
fn hello_server(
    server: hyper::server::conn::http1::Builder,
) -> impl Fn(Remote<ServerAddr>) -> io::Result<io::BoxedIo> {
    move |endpoint| {
        let span = tracing::info_span!("hello_server", ?endpoint);
        let _e = span.enter();
        tracing::info!("mock connecting");
        let (client_io, server_io) = support::io::duplex(4096);
        let hello_svc =
            hyper::service::service_fn(|request: Request<hyper::body::Incoming>| async move {
                tracing::info!(?request);
                Ok::<_, io::Error>(Response::new(BoxBody::from_static("Hello world!")))
            });
        tokio::spawn(
            server
                .serve_connection(hyper_util::rt::TokioIo::new(server_io), hello_svc)
                .in_current_span(),
        );
        Ok(io::BoxedIo::new(client_io))
    }
}

#[tracing::instrument]
fn grpc_status_server(
    server: hyper::server::conn::http2::Builder<TracingExecutor>,
    status: tonic::Code,
) -> impl Fn(Remote<ServerAddr>) -> io::Result<io::BoxedIo> {
    move |endpoint| {
        let span = tracing::info_span!("grpc_status_server", ?endpoint);
        let _e = span.enter();
        tracing::info!("mock connecting");
        let (client_io, server_io) = support::io::duplex(4096);
        tokio::spawn(
            server
                .serve_connection(
                    hyper_util::rt::TokioIo::new(server_io),
                    hyper::service::service_fn(
                        move |request: Request<hyper::body::Incoming>| async move {
                            tracing::info!(?request);
                            let (mut tx, rx) =
                                http_body_util::channel::Channel::<bytes::Bytes, Error>::new(1024);
                            tokio::spawn(async move {
                                let mut trls = ::http::HeaderMap::new();
                                trls.insert(
                                    "grpc-status",
                                    (status as u32).to_string().parse().unwrap(),
                                );
                                tx.send_trailers(trls).await
                            });
                            Ok::<_, io::Error>(
                                http::Response::builder()
                                    .version(::http::Version::HTTP_2)
                                    .header("content-type", "application/grpc")
                                    .body(rx)
                                    .unwrap(),
                            )
                        },
                    ),
                )
                .in_current_span(),
        );
        Ok(io::BoxedIo::new(client_io))
    }
}

#[tracing::instrument]
fn connect_error() -> impl Fn(Remote<ServerAddr>) -> io::Result<io::BoxedIo> {
    move |_| {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "server is not listening",
        ))
    }
}

#[tracing::instrument]
fn connect_timeout() -> Box<dyn FnMut(Remote<ServerAddr>) -> ConnectFuture + Send> {
    Box::new(move |endpoint| {
        let span = tracing::info_span!("connect_timeout", ?endpoint);
        Box::pin(
            async move {
                tracing::info!("sleeping so that the proxy hits a connect timeout");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                // The proxy hits a connect timeout so we don't need to worry
                // about returning a service here.
                unreachable!();
            }
            .instrument(span.or_current()),
        )
    })
}

#[derive(Clone, Debug)]
struct Target(http::Variant, tls::ConditionalServerTls);

#[track_caller]
fn check_error_header(hdrs: &::http::HeaderMap, expected: &str) {
    let message = hdrs
        .get(L5D_PROXY_ERROR)
        .expect("response did not contain l5d-proxy-error header")
        .to_str()
        .expect("l5d-proxy-error header should always be a UTF-8 string");
    assert!(
        message.contains(expected),
        "expected {L5D_PROXY_ERROR} to contain {expected:?}; got: {message:?}",
    );
}

// === impl Target ===

impl Target {
    const UNMESHED_HTTP1: Self = Self(
        http::Variant::Http1,
        tls::ConditionalServerTls::None(tls::NoServerTls::NoClientHello),
    );
    const UNMESHED_H2: Self = Self(
        http::Variant::H2,
        tls::ConditionalServerTls::None(tls::NoServerTls::NoClientHello),
    );

    fn meshed_http1() -> Self {
        Self(
            http::Variant::Http1,
            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(tls::ClientId(
                    "foosa.barns.serviceaccount.identity.linkerd.cluster.local"
                        .parse()
                        .unwrap(),
                )),
                negotiated_protocol: None,
            }),
        )
    }

    fn meshed_h2() -> Self {
        Self(
            http::Variant::H2,
            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(tls::ClientId(
                    "foosa.barns.serviceaccount.identity.linkerd.cluster.local"
                        .parse()
                        .unwrap(),
                )),
                negotiated_protocol: None,
            }),
        )
    }

    fn addr() -> SocketAddr {
        ([127, 0, 0, 1], 80).into()
    }
}

impl svc::Param<OrigDstAddr> for Target {
    fn param(&self) -> OrigDstAddr {
        OrigDstAddr(([192, 0, 2, 2], 80).into())
    }
}

impl svc::Param<Remote<ServerAddr>> for Target {
    fn param(&self) -> Remote<ServerAddr> {
        Remote(ServerAddr(Self::addr()))
    }
}

impl svc::Param<Remote<ClientAddr>> for Target {
    fn param(&self) -> Remote<ClientAddr> {
        Remote(ClientAddr(([192, 0, 2, 3], 50000).into()))
    }
}

impl svc::Param<http::Variant> for Target {
    fn param(&self) -> http::Variant {
        self.0
    }
}

impl svc::Param<tls::ConditionalServerTls> for Target {
    fn param(&self) -> tls::ConditionalServerTls {
        self.1.clone()
    }
}

impl svc::Param<policy::AllowPolicy> for Target {
    fn param(&self) -> policy::AllowPolicy {
        let authorizations = Arc::new([policy::Authorization {
            authentication: policy::Authentication::Unauthenticated,
            networks: vec![std::net::IpAddr::from([192, 0, 2, 3]).into()],
            meta: Arc::new(policy::Meta::Resource {
                group: "policy.linkerd.io".into(),
                kind: "serverauthorization".into(),
                name: "testsaz".into(),
            }),
        }]);
        let (policy, _) = policy::AllowPolicy::for_test(
            self.param(),
            policy::ServerPolicy {
                protocol: policy::Protocol::Http1(Arc::new([
                    linkerd_proxy_server_policy::http::default(authorizations),
                ])),
                meta: Arc::new(policy::Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "server".into(),
                    name: "testsrv".into(),
                }),
                local_rate_limit: Default::default(),
            },
        );
        policy
    }
}

impl svc::Param<policy::ServerLabel> for Target {
    fn param(&self) -> policy::ServerLabel {
        policy::ServerLabel(
            Arc::new(policy::Meta::Resource {
                group: "policy.linkerd.io".into(),
                kind: "server".into(),
                name: "testsrv".into(),
            }),
            80,
        )
    }
}

impl svc::Param<http::normalize_uri::DefaultAuthority> for Target {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(None)
    }
}

impl svc::Param<Option<identity::Id>> for Target {
    fn param(&self) -> Option<identity::Id> {
        None
    }
}
