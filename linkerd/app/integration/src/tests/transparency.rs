use crate::*;
use std::error::Error as _;
use tokio::sync::mpsc;
use tokio::time::timeout;

#[tokio::test]
async fn outbound_http1() {
    let _trace = trace_init();

    let srv = server::http1().route("/", "hello h1").run().await;
    let ctrl = controller::new();
    let _profile = ctrl.profile_tx_default(srv.addr, "transparency.test.svc.cluster.local");
    let dest = ctrl.destination_tx(format!(
        "transparency.test.svc.cluster.local:{}",
        srv.addr.port()
    ));
    dest.send_addr(srv.addr);
    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .outbound(srv)
        .run()
        .await;
    let client = client::http1(proxy.outbound, "transparency.test.svc.cluster.local");

    assert_eq!(client.get("/").await, "hello h1");
    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

#[tokio::test]
async fn inbound_http1() {
    let _trace = trace_init();

    let srv = server::http1().route("/", "hello h1").run().await;
    let ctrl = controller::new();
    let _profile = ctrl.profile_tx_default(srv.addr, "transparency.test.svc.cluster.local");
    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .inbound(srv)
        .run()
        .await;
    let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

    assert_eq!(client.get("/").await, "hello h1");
    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

#[tokio::test]
async fn outbound_tcp() {
    let _trace = trace_init();

    let msg1 = "custom tcp hello\n";
    let msg2 = "custom tcp bye";

    let srv = server::tcp()
        .accept(move |read| {
            assert_eq!(read, msg1.as_bytes());
            msg2
        })
        .run()
        .await;
    let ctrl = controller::new();
    let _profile = ctrl.profile_tx_default(srv.addr, &srv.addr.to_string());
    let dest = ctrl.destination_tx(&srv.addr.to_string());
    dest.send_addr(srv.addr);
    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .outbound(srv)
        .run()
        .await;

    let client = client::tcp(proxy.outbound);

    let tcp_client = client.connect().await;

    tcp_client.write(msg1).await;
    assert_eq!(tcp_client.read().await, msg2.as_bytes());

    // TCP client must close first
    tcp_client.shutdown().await;

    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

#[tokio::test]
async fn outbound_tcp_external() {
    let _trace = trace_init();

    let msg1 = "custom tcp hello\n";
    let msg2 = "custom tcp bye";

    let srv = server::tcp()
        .accept(move |read| {
            assert_eq!(read, msg1.as_bytes());
            msg2
        })
        .run()
        .await;
    let ctrl = controller::new();
    let profile = ctrl.profile_tx(&srv.addr.to_string());
    profile.send_err(grpc::Status::invalid_argument(
        "we're pretending this is outside of the cluster",
    ));
    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .outbound(srv)
        .run()
        .await;

    let client = client::tcp(proxy.outbound);

    let tcp_client = client.connect().await;

    tcp_client.write(msg1).await;
    assert_eq!(tcp_client.read().await, msg2.as_bytes());

    // TCP client must close first
    tcp_client.shutdown().await;

    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

#[tokio::test]
async fn inbound_tcp() {
    let _trace = trace_init();

    let msg1 = "custom tcp hello\n";
    let msg2 = "custom tcp bye";

    let srv = server::tcp()
        .accept(move |read| {
            assert_eq!(read, msg1.as_bytes());
            msg2
        })
        .run()
        .await;
    let proxy = proxy::new().inbound(srv).run().await;

    let client = client::tcp(proxy.inbound);

    let tcp_client = client.connect().await;

    tcp_client.write(msg1).await;
    assert_eq!(tcp_client.read().await, msg2.as_bytes());

    // TCP client must close first
    tcp_client.shutdown().await;

    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

#[tokio::test]
#[cfg_attr(not(feature = "flaky_tests"), ignore)]
async fn loop_outbound_http1() {
    let _trace = trace_init();

    let listen_addr = SocketAddr::from(([127, 0, 0, 1], 10751));

    let mut env = TestEnv::default();
    env.put(app::env::ENV_OUTBOUND_LISTEN_ADDR, listen_addr.to_string());
    let _proxy = proxy::new()
        .outbound_ip(listen_addr)
        .run_with_test_env_and_keep_ports(env);

    let client = client::http1(listen_addr, "some.invalid.example.com");
    let rsp = client
        .request(client.request_builder("/").method("GET"))
        .await
        .unwrap();
    assert_eq!(rsp.status(), http::StatusCode::BAD_GATEWAY);
}

#[tokio::test]
#[cfg_attr(not(feature = "flaky_tests"), ignore)]
async fn loop_inbound_http1() {
    let _trace = trace_init();

    let listen_addr = SocketAddr::from(([127, 0, 0, 1], 10752));

    let mut env = TestEnv::default();
    env.put(app::env::ENV_INBOUND_LISTEN_ADDR, listen_addr.to_string());
    let _proxy = proxy::new()
        .inbound_ip(listen_addr)
        .run_with_test_env_and_keep_ports(env);

    let client = client::http1(listen_addr, listen_addr.to_string());
    let rsp = client
        .request(client.request_builder("/").method("GET"))
        .await
        .unwrap();
    assert_eq!(rsp.status(), http::StatusCode::FORBIDDEN);
}

async fn test_server_speaks_first(env: TestEnv) {
    const TIMEOUT: Duration = Duration::from_secs(5);

    let _trace = trace_init();

    let msg1 = "custom tcp server starts";
    let msg2 = "custom tcp client second";

    let (tx, mut rx) = mpsc::channel(1);
    let srv = server::tcp()
        .accept_fut(move |mut sock| {
            async move {
                sock.write_all(msg1.as_bytes()).await?;
                let mut vec = vec![0; 512];
                let n = sock.read(&mut vec).await?;
                assert_eq!(s(&vec[..n]), msg2);
                tx.send(()).await.unwrap();
                Ok::<(), io::Error>(())
            }
            .map(|res| res.expect("TCP server must not fail"))
        })
        .run()
        .await;

    let proxy = proxy::new()
        .disable_inbound_ports_protocol_detection(vec![srv.addr.port()])
        .inbound(srv)
        .run_with_test_env(env)
        .await;

    let client = client::tcp(proxy.inbound);

    let tcp_client = client.connect().await;

    assert_eq!(s(&tcp_client.read_timeout(TIMEOUT).await), msg1);
    tcp_client.write(msg2).await;
    timeout(TIMEOUT, rx.recv()).await.unwrap();

    // TCP client must close first
    tcp_client.shutdown().await;

    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

#[tokio::test]
async fn tcp_server_first() {
    test_server_speaks_first(TestEnv::default()).await;
}

#[tokio::test]
async fn tcp_server_first_tls() {
    use std::path::PathBuf;

    let (_cert, _key, _trust_anchors) = {
        let path_to_string = |path: &PathBuf| {
            path.as_path()
                .to_owned()
                .into_os_string()
                .into_string()
                .unwrap()
        };
        let mut tls = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        tls.push("src");
        tls.push("transport");
        tls.push("tls");
        tls.push("testdata");

        tls.push("foo-ns1-ca1.crt");
        let cert = path_to_string(&tls);

        tls.set_file_name("foo-ns1-ca1.p8");
        let key = path_to_string(&tls);

        tls.set_file_name("ca1.pem");
        let trust_anchors = path_to_string(&tls);
        (cert, key, trust_anchors)
    };

    let env = TestEnv::default();

    // FIXME
    //env.put(app::env::ENV_TLS_CERT, cert);
    //env.put(app::env::ENV_TLS_PRIVATE_KEY, key);
    //env.put(app::env::ENV_TLS_TRUST_ANCHORS, trust_anchors);
    //env.put(
    //    app::env::ENV_TLS_LOCAL_IDENTITY,
    //    "foo.deployment.ns1.linkerd-managed.linkerd.svc.cluster.local".to_string(),
    //);

    test_server_speaks_first(env).await
}

#[tokio::test]
#[allow(warnings)]
async fn tcp_connections_close_if_client_closes() {
    let _trace = trace_init();

    let msg1 = "custom tcp hello\n";
    let msg2 = "custom tcp bye";

    let (mut tx, mut rx) = mpsc::channel(1);

    let srv = server::tcp()
        .accept_fut(move |mut sock| {
            async move {
                let mut vec = vec![0; 1024];
                let n = sock.read(&mut vec).await?;

                assert_eq!(s(&vec[..n]), msg1);
                sock.write_all(msg2.as_bytes()).await?;
                let n = sock.read(&mut [0; 16]).await?;
                assert_eq!(n, 0);
                panic!("lol");
                tx.send(()).await.unwrap();
                Ok::<(), io::Error>(())
            }
            .map(|res| res.expect("TCP server must not fail"))
        })
        .run()
        .await;
    let proxy = proxy::new().inbound(srv).run().await;

    let client = client::tcp(proxy.inbound);

    let tcp_client = client.connect().await;
    tcp_client.write(msg1).await;
    assert_eq!(s(&tcp_client.read().await[..]), msg2);

    tcp_client.shutdown().await;

    // rx will be fulfilled when our tcp accept_fut sees
    // a socket disconnect, which is what we are testing for.
    // the timeout here is just to prevent this test from hanging
    timeout(Duration::from_secs(5), rx.recv()).await.unwrap();

    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

macro_rules! http1_tests {
    (proxy: $proxy:expr) => {
        #[tokio::test]
        async fn inbound_http1() {
            let _trace = trace_init();

            let srv = server::http1().route("/", "hello h1").run().await;
            let mk = $proxy;
            let proxy = mk(srv).await;
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            assert_eq!(client.get("/").await, "hello h1");

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn http1_removes_connection_headers() {
            let _trace = trace_init();

            let srv = server::http1()
                .route_fn("/", |req| {
                    assert!(!req.headers().contains_key("x-foo-bar"));
                    Response::builder()
                        .header("x-server-quux", "lorem ipsum")
                        .header("connection", "close, x-server-quux")
                        .header("keep-alive", "500")
                        .header("proxy-connection", "a")
                        .body(Default::default())
                        .unwrap()
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let res = client
                .request(
                    client
                        .request_builder("/")
                        .header("x-foo-bar", "baz")
                        .header("connection", "x-foo-bar, close")
                        // These headers will fail in the proxy_to_proxy case if
                        // they are not stripped.
                        //
                        // normally would be stripped by `connection: keep-alive`,
                        // but test its removed even if the connection header forgot
                        // about it.
                        .header("keep-alive", "500")
                        .header("proxy-connection", "a"),
                )
                .await
                .unwrap();

            assert_eq!(res.status(), http::StatusCode::OK);
            assert!(!res.headers().contains_key("x-server-quux"));

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn http10_with_host() {
            let _trace = trace_init();

            let host = "transparency.test.svc.cluster.local";
            let srv = server::http1()
                .route_fn("/", move |req| {
                    assert_eq!(req.version(), http::Version::HTTP_10);
                    assert_eq!(req.headers().get("host").unwrap(), host);
                    Response::builder()
                        .version(http::Version::HTTP_10)
                        .body("".into())
                        .unwrap()
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;
            let client = client::http1(proxy.inbound, host);

            let res = client
                .request(
                    client
                        .request_builder("/")
                        .version(http::Version::HTTP_10)
                        .header("host", host),
                )
                .await
                .unwrap();

            assert_eq!(res.status(), http::StatusCode::OK);
            assert_eq!(res.version(), http::Version::HTTP_10);

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn http11_absolute_uri_differs_from_host() {
            let _trace = trace_init();

            // We shouldn't touch the URI or the Host, just pass directly as we got.
            let auth = "transparency.test.svc.cluster.local";
            let host = "foo.bar";
            let srv = server::http1()
                .route_fn("/", move |req| {
                    assert_eq!(req.headers()["host"], host);
                    assert_eq!(req.uri().to_string(), format!("http://{}/", auth));
                    Response::new("".into())
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;
            let client = client::http1_absolute_uris(proxy.inbound, auth);

            let res = client
                .request(
                    client
                        .request_builder("/")
                        .version(http::Version::HTTP_11)
                        .header("host", host),
                )
                .await
                .unwrap();

            assert_eq!(res.status(), http::StatusCode::OK);
            assert_eq!(res.version(), http::Version::HTTP_11);

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn http11_upgrades() {
            let _trace = trace_init();

            // To simplify things for this test, we just use the test TCP
            // client and server to do an HTTP upgrade.
            //
            // This is upgrading to 'chatproto', a made up plaintext protocol
            // to simplify testing.

            let upgrade_req = "\
                               GET /chat HTTP/1.1\r\n\
                               Host: transparency.test.svc.cluster.local\r\n\
                               Connection: upgrade\r\n\
                               Upgrade: chatproto\r\n\
                               \r\n\
                               ";
            let upgrade_res = "\
                               HTTP/1.1 101 Switching Protocols\r\n\
                               Upgrade: chatproto\r\n\
                               Connection: upgrade\r\n\
                               \r\n\
                               ";
            let upgrade_needle = "\r\nupgrade: chatproto\r\n";
            let chatproto_req = "[chatproto-c]{send}: hi all\n";
            let chatproto_res = "[chatproto-s]{recv}: welcome!\n";

            let srv = server::tcp()
                .accept_fut(move |mut sock| {
                    async move {
                        // Read upgrade_req...
                        let mut vec = vec![0; 512];
                        let n = sock.read(&mut vec).await?;
                        assert_contains!(s(&vec[..n]), upgrade_needle);

                        // Write upgrade_res back...
                        sock.write_all(upgrade_res.as_bytes()).await?;

                        // Read the message in 'chatproto' format
                        let mut vec = vec![0; 512];
                        let n = sock.read(&mut vec).await?;
                        assert_eq!(s(&vec[..n]), chatproto_req);

                        // Some processing... and then write back in chatproto...
                        sock.write_all(chatproto_res.as_bytes()).await
                    }
                    .map(|res| res.expect("TCP server must not fail"))
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;

            let client = client::tcp(proxy.inbound);

            let tcp_client = client.connect().await;

            tcp_client.write(upgrade_req).await;

            let resp = tcp_client.read().await;
            let resp_str = s(&resp);
            assert!(
                resp_str.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
                "response not an upgrade: {:?}",
                resp_str
            );
            assert_contains!(resp_str, upgrade_needle);

            // We've upgraded from HTTP to chatproto! Say hi!
            tcp_client.write(chatproto_req).await;
            // Did anyone respond?
            let chat_resp = tcp_client.read().await;
            assert_eq!(s(&chat_resp), chatproto_res);

            // TCP client must close first
            tcp_client.shutdown().await;

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn l5d_orig_proto_header_isnt_leaked() {
            let _trace = trace_init();

            let srv = server::http1()
                .route_fn("/", |req| {
                    assert_eq!(req.headers().get("l5d-orig-proto"), None, "request");
                    Response::new(Default::default())
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;

            let host = "transparency.test.svc.cluster.local";
            let client = client::http1(proxy.inbound, host);

            let res = client.request(client.request_builder("/")).await.unwrap();
            assert_eq!(res.status(), 200);
            assert_eq!(res.headers().get("l5d-orig-proto"), None, "response");

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn http11_upgrade_h2_stripped() {
            let _trace = trace_init();

            // If an `h2` upgrade over HTTP/1.1 were to go by the proxy,
            // and it succeeded, there would an h2 connection, but it would
            // be opaque-to-the-proxy, acting as just a TCP proxy.
            //
            // A user wouldn't be able to see any usual HTTP telemetry about
            // requests going over that connection. Instead of that confusion,
            // the proxy strips h2 upgrade headers.
            //
            // Eventually, the proxy will support h2 upgrades directly.

            let srv = server::http1()
                .route_fn("/", |req| {
                    assert!(!req.headers().contains_key("connection"));
                    assert!(!req.headers().contains_key("upgrade"));
                    assert!(!req.headers().contains_key("http2-settings"));
                    Response::default()
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;

            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let res = client
                .request(
                    client
                        .request_builder("/")
                        .header("upgrade", "h2c")
                        .header("http2-settings", "")
                        .header("connection", "upgrade, http2-settings"),
                )
                .await
                .unwrap();

            // If the assertion is trigger in the above test route, the proxy will
            // just send back a 500.
            assert_eq!(res.status(), http::StatusCode::OK);

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn http11_connect() {
            let _trace = trace_init();

            // To simplify things for this test, we just use the test TCP
            // client and server to do an HTTP CONNECT.
            //
            // We don't *actually* perfom a new connect to requested host,
            // but client doesn't need to know that for our tests.

            let connect_req = b"\
                               CONNECT transparency.test.svc.cluster.local HTTP/1.1\r\n\
                               Host: transparency.test.svc.cluster.local\r\n\
                               \r\n\
                               ";
            let connect_res = b"\
                               HTTP/1.1 200 OK\r\n\
                               \r\n\
                               ";

            let tunneled_req = b"{send}: hi all\n";
            let tunneled_res = b"{recv}: welcome!\n";

            let srv = server::tcp()
                .accept_fut(move |mut sock| {
                    async move {
                        // Read connect_req...
                        let mut vec = vec![0; 512];
                        let n = sock.read(&mut vec).await?;
                        let head = s(&vec[..n]);
                        assert_contains!(
                            head,
                            "CONNECT transparency.test.svc.cluster.local HTTP/1.1\r\n"
                        );

                        // Write connect_res back...
                        sock.write_all(&connect_res[..]).await?;

                        // Read the message after tunneling...
                        let mut vec = vec![0; 512];
                        let n = sock.read(&mut vec).await?;
                        assert_eq!(s(&vec[..n]), s(&tunneled_req[..]));

                        // Some processing... and then write back tunneled res...
                        sock.write_all(&tunneled_res[..]).await
                    }
                    .map(|res: std::io::Result<()>| match res {
                        Ok(()) => {}
                        Err(e) => panic!("tcp server error: {}", e),
                    })
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;

            let client = client::tcp(proxy.inbound);

            let tcp_client = client.connect().await;

            tcp_client.write(&connect_req[..]).await;

            let resp = tcp_client.read().await;
            let resp_str = s(&resp);
            assert!(
                resp_str.starts_with("HTTP/1.1 200 OK\r\n"),
                "response not an upgrade: {:?}",
                resp_str
            );

            // We've CONNECTed from HTTP to foo.bar! Say hi!
            tcp_client.write(&tunneled_req[..]).await;
            // Did anyone respond?
            let resp2 = tcp_client.read().await;
            assert_eq!(s(&resp2), s(&tunneled_res[..]));

            // TCP client must close first
            tcp_client.shutdown().await;

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn http11_connect_bad_requests() {
            let _trace = trace_init();

            let srv = server::tcp()
                .accept(move |_sock| -> Vec<u8> {
                    unreachable!("shouldn't get through the proxy");
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;

            // A TCP client is used since the HTTP client would stop these requests
            // from ever touching the network.
            let client = client::tcp(proxy.inbound);

            let bad_uris = vec!["/origin-form", "/", "http://test/bar", "http://test", "*"];

            for bad_uri in bad_uris {
                let tcp_client = client.connect().await;

                let req = format!("CONNECT {} HTTP/1.1\r\nHost: test\r\n\r\n", bad_uri);
                tcp_client.write(req).await;

                let resp = tcp_client.read().await;
                let resp_str = s(&resp);
                assert!(
                    resp_str.starts_with("HTTP/1.1 400 Bad Request\r\n"),
                    "bad URI ({:?}) should get 400 response: {:?}",
                    bad_uri,
                    resp_str
                );
            }

            // origin-form URIs must be CONNECT
            let tcp_client = client.connect().await;

            tcp_client
                .write("GET test HTTP/1.1\r\nHost: test\r\n\r\n")
                .await;

            let resp = tcp_client.read().await;
            let resp_str = s(&resp);
            assert!(
                resp_str.starts_with("HTTP/1.1 400 Bad Request\r\n"),
                "origin-form without CONNECT should get 400 response: {:?}",
                resp_str
            );

            // check that HTTP/1.0 is not allowed for CONNECT
            let tcp_client = client.connect().await;

            tcp_client
                .write("CONNECT test HTTP/1.0\r\nHost: test\r\n\r\n")
                .await;

            let resp = tcp_client.read().await;
            let resp_str = s(&resp);
            assert!(
                resp_str.starts_with("HTTP/1.0 400 Bad Request\r\n"),
                "HTTP/1.0 CONNECT should get 400 response: {:?}",
                resp_str
            );

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn http1_request_with_body_content_length() {
            let _trace = trace_init();

            let srv = server::http1()
                .route_fn("/", |req| {
                    assert_eq!(req.headers()["content-length"], "5");
                    Response::default()
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let req = client
                .request_builder("/")
                .method("POST")
                .body("hello".into())
                .unwrap();

            let resp = client.request_body(req).await;

            assert_eq!(resp.status(), StatusCode::OK);

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn http1_request_with_body_chunked() {
            let _trace = trace_init();

            let srv = server::http1()
                .route_async("/", |req| async move {
                    assert_eq!(req.headers()["transfer-encoding"], "chunked");
                    let body = http_util::body_to_string(req.into_body()).await.unwrap();
                    assert_eq!(body, "hello");
                    Ok::<_, std::io::Error>(
                        Response::builder()
                            .header("transfer-encoding", "chunked")
                            .body("world".into())
                            .unwrap(),
                    )
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let req = client
                .request_builder("/")
                .method("POST")
                .header("transfer-encoding", "chunked")
                .body("hello".into())
                .unwrap();

            let resp = client.request_body(req).await;

            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(resp.headers()["transfer-encoding"], "chunked");
            let body = http_util::body_to_string(resp.into_body()).await.unwrap();
            assert_eq!(body, "world");

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn http1_requests_without_body_doesnt_add_transfer_encoding() {
            let _trace = trace_init();

            let srv = server::http1()
                .route_fn("/", |req| {
                    let status = if req.headers().contains_key(http::header::TRANSFER_ENCODING) {
                        StatusCode::BAD_REQUEST
                    } else {
                        StatusCode::OK
                    };
                    let mut res = Response::new("".into());
                    *res.status_mut() = status;
                    res
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let methods = &["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];

            for &method in methods {
                let resp = client
                    .request(client.request_builder("/").method(method))
                    .await
                    .unwrap();

                assert_eq!(resp.status(), StatusCode::OK, "method={:?}", method);
            }

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn http1_content_length_zero_is_preserved() {
            let _trace = trace_init();

            let srv = server::http1()
                .route_fn("/", |req| {
                    let status = if req.headers()["content-length"] == "0" {
                        StatusCode::OK
                    } else {
                        StatusCode::BAD_REQUEST
                    };
                    Response::builder()
                        .status(status)
                        .header("content-length", "0")
                        .body("".into())
                        .unwrap()
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let methods = &["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];

            for &method in methods {
                let resp = client
                    .request(
                        client
                            .request_builder("/")
                            .method(method)
                            .header("content-length", "0"),
                    )
                    .await
                    .unwrap();

                assert_eq!(resp.status(), StatusCode::OK, "method={:?}", method);
                assert_eq!(resp.headers()["content-length"], "0", "method={:?}", method);
            }
        }

        #[tokio::test]
        async fn http1_bodyless_responses() {
            let _trace = trace_init();

            let req_status_header = "x-test-status-requested";

            let srv = server::http1()
                .route_fn("/", move |req| {
                    let status = req
                        .headers()
                        .get(req_status_header)
                        .map(|val| {
                            val.to_str()
                                .expect("req_status_header should be ascii")
                                .parse::<u16>()
                                .expect("req_status_header should be numbers")
                        })
                        .unwrap_or(200);

                    Response::builder().status(status).body("".into()).unwrap()
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            // https://tools.ietf.org/html/rfc7230#section-3.3.3
            // > response to a HEAD request, any 1xx, 204, or 304 cannot contain a body

            //TODO: the proxy doesn't support CONNECT requests yet, but when we do,
            //they should be tested here as well. As RFC7230 says, a 2xx response to
            //a CONNECT request is not allowed to contain a body (but 4xx, 5xx can!).

            let resp = client
                .request(client.request_builder("/").method("HEAD"))
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::OK);
            assert!(!resp.headers().contains_key("transfer-encoding"));

            let statuses = &[
                //TODO: test some 1xx status codes.
                //The current test server doesn't support sending 1xx responses
                //easily. We could test this by making a new unit test with the
                //server being a TCP server, and write the response manually.
                StatusCode::NO_CONTENT,   // 204
                StatusCode::NOT_MODIFIED, // 304
            ];

            for &status in statuses {
                let resp = client
                    .request(
                        client
                            .request_builder("/")
                            .header(req_status_header, status.as_str()),
                    )
                    .await
                    .unwrap();

                assert_eq!(resp.status(), status);
                assert!(
                    !resp.headers().contains_key("transfer-encoding"),
                    "transfer-encoding with status={:?}",
                    status
                );
            }

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn http1_head_responses() {
            let _trace = trace_init();

            let srv = server::http1()
                .route_fn("/", move |req| {
                    assert_eq!(req.method(), "HEAD");
                    Response::builder()
                        .header("content-length", "55")
                        .body("".into())
                        .unwrap()
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let resp = client
                .request(client.request_builder("/").method("HEAD"))
                .await
                .expect("request");

            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(resp.headers()["content-length"], "55");

            let body = http_util::body_to_string(resp.into_body()).await.unwrap();

            assert_eq!(body, "");

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn http1_response_end_of_file() {
            let _trace = trace_init();

            // test both http/1.0 and 1.1
            let srv = server::tcp()
                .accept(move |_read| {
                    "\
                     HTTP/1.0 200 OK\r\n\
                     \r\n\
                     body till eof\
                     "
                })
                .accept(move |_read| {
                    "\
                     HTTP/1.1 200 OK\r\n\
                     \r\n\
                     body till eof\
                     "
                })
                .run()
                .await;
            let mk = $proxy;
            let proxy = mk(srv).await;

            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let versions = &[
                "1.0",
                // TODO: We may wish to enforce not translating eof bodies to chunked,
                // even if the client is 1.1. However, there also benefits of translating:
                // the client can reuse the connection, and delimited messages are much
                // safer than those that end with the connection (as it's difficult to
                // notice if a full response was received).
                //
                // Either way, hyper's server does not provide the ability to do this,
                // so we cannot test for it at the moment.
                //"1.1",
            ];

            for v in versions {
                let resp = client
                    .request(client.request_builder("/").method("GET"))
                    .await
                    .expect("response");

                assert_eq!(resp.status(), StatusCode::OK, "HTTP/{}", v);
                assert!(
                    !resp.headers().contains_key("transfer-encoding"),
                    "HTTP/{} transfer-encoding",
                    v
                );
                assert!(
                    !resp.headers().contains_key("content-length"),
                    "HTTP/{} content-length",
                    v
                );

                let body = http_util::body_to_string(resp.into_body()).await.unwrap();

                assert_eq!(body, "body till eof", "HTTP/{} body", v);
            }

            // ensure panics from the server are propagated
            proxy.join_servers().await;
        }
    };
}

mod one_proxy {
    use crate::*;

    http1_tests! { proxy: |srv: server::Listening| async move {
        let ctrl = controller::new();
        let _profile = ctrl.profile_tx_default(srv.addr, "transparency.test.svc.cluster.local");
        proxy::new().inbound(srv).controller(ctrl.run().await).run().await
    }}
}

mod proxy_to_proxy {
    use crate::*;

    struct ProxyToProxy {
        in_proxy: proxy::Listening,
        out_proxy: proxy::Listening,
        inbound: SocketAddr,

        // These are all held to prevent closing, to reduce controller request
        // noise during tests
        _dst: controller::DstSender,
        _profile_in: controller::ProfileSender,
        _profile_out: controller::ProfileSender,
    }

    impl ProxyToProxy {
        async fn join_servers(self) {
            tokio::join! {
                self.in_proxy.join_servers(),
                self.out_proxy.join_servers(),
            };
        }
    }

    http1_tests! { proxy: |srv: server::Listening| async move {
        let ctrl = controller::new();
        let srv_addr = srv.addr;
        let dst = format!("transparency.test.svc.cluster.local:{}", srv_addr.port());
        let _profile_in = ctrl.profile_tx_default(&dst, "transparency.test.svc.cluster.local");
        let ctrl = ctrl
            .run()
            .instrument(tracing::info_span!("ctrl", "inbound"))
            .await;
        let inbound = proxy::new().controller(ctrl).inbound(srv).run().await;

        let ctrl = controller::new();
        let _profile_out = ctrl.profile_tx_default(srv_addr, "transparency.test.svc.cluster.local");
        let dst = ctrl.destination_tx(dst);
        dst.send_h2_hinted(inbound.inbound);

        let ctrl = ctrl
            .run()
            .instrument(tracing::info_span!("ctrl", "outbound"))
            .await;
        let outbound = proxy::new()
            .controller(ctrl)
            .outbound_ip(srv_addr)
            .run().await;

        let addr = outbound.outbound;
        ProxyToProxy {
            _dst: dst,
            _profile_in,
            _profile_out,
            in_proxy: inbound,
            out_proxy: outbound,
            inbound: addr,
        }
    } }
}

#[tokio::test]
async fn http10_without_host() {
    // Without a host or authority, there's no way to route this test,
    // so its not part of the proxy_to_proxy set.
    let _trace = trace_init();

    let srv = server::http1()
        .route_fn("/", move |req| {
            assert_eq!(req.version(), http::Version::HTTP_10);
            assert!(!req.headers().contains_key("host"));
            assert_eq!(req.uri().to_string(), "/");
            Response::builder()
                .version(http::Version::HTTP_10)
                .body("".into())
                .unwrap()
        })
        .run()
        .await;
    let proxy = proxy::new().inbound(srv).run().await;

    let client = client::tcp(proxy.inbound);

    let tcp_client = client.connect().await;

    tcp_client
        .write(
            "\
         GET / HTTP/1.0\r\n\
         \r\n\
         ",
        )
        .await;

    let expected = "HTTP/1.0 200 OK\r\n";
    assert_eq!(s(&tcp_client.read().await[..expected.len()]), expected);

    proxy.join_servers().await;
}

#[tokio::test]
async fn http1_one_connection_per_host() {
    let _trace = trace_init();

    let srv = server::http1()
        .route("/body", "hello hosts")
        .route("/no-body", "")
        .run()
        .await;
    let proxy = proxy::new().inbound(srv).run().await;

    let client = client::http1(proxy.inbound, "foo.bar");

    let inbound = &proxy.inbound_server.as_ref().expect("no inbound server!");

    // Run each case with and without a body.
    let run_request = |host, expected_conn_cnt| {
        let c = &client;
        let i = &inbound;
        async move {
            for path in &["/no-body", "/body"][..] {
                let res = c
                    .request(
                        c.request_builder(path)
                            .version(http::Version::HTTP_11)
                            .header("host", host),
                    )
                    .await
                    .unwrap();
                assert_eq!(res.status(), http::StatusCode::OK);
                assert_eq!(res.version(), http::Version::HTTP_11);
                assert_eq!(i.connections(), expected_conn_cnt);
            }
        }
    };

    // Make a request with the header "Host: foo.bar". After the request, the
    // server should have seen one connection.
    run_request("foo.bar", 1).await;

    // Another request with the same host. The proxy may reuse the connection.
    run_request("foo.bar", 1).await;

    // Make a request with a different Host header. This request must use a new
    // connection.
    run_request("bar.baz", 2).await;
    run_request("bar.baz", 2).await;

    // Make a request with a different Host header. This request must use a new
    // connection.
    run_request("quuuux.com", 3).await;

    proxy.join_servers().await;
}

#[tokio::test]
async fn http1_requests_without_host_have_unique_connections() {
    let _trace = trace_init();

    let srv = server::http1().route("/", "unique hosts").run().await;
    let proxy = proxy::new().inbound(srv).run().await;

    let client = client::http1(proxy.inbound, "foo.bar");

    let inbound = &proxy.inbound_server.as_ref().expect("no inbound server!");

    // Make a request with no Host header and no authority in the request path.
    let res = client
        .request(
            client
                .request_builder("/")
                .version(http::Version::HTTP_11)
                .header("host", ""),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.version(), http::Version::HTTP_11);
    assert_eq!(inbound.connections(), 1);

    // Another request with no Host. The proxy must open a new connection
    // for that request.
    let res = client
        .request(
            client
                .request_builder("/")
                .version(http::Version::HTTP_11)
                .header("host", ""),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.version(), http::Version::HTTP_11);
    assert_eq!(inbound.connections(), 2);

    // Make a request with a host header. It must also receive its
    // own connection.
    let res = client
        .request(
            client
                .request_builder("/")
                .version(http::Version::HTTP_11)
                .header("host", "foo.bar"),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.version(), http::Version::HTTP_11);
    assert_eq!(inbound.connections(), 3);

    // Another request with no Host. The proxy must open a new connection
    // for that request.
    let res = client
        .request(
            client
                .request_builder("/")
                .version(http::Version::HTTP_11)
                .header("host", ""),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.version(), http::Version::HTTP_11);
    assert_eq!(inbound.connections(), 4);

    proxy.join_servers().await;
}

#[tokio::test]
async fn retry_reconnect_errors() {
    let _trace = trace_init();

    // Used to delay `listen` in the server, to force connection refused errors.
    let (tx, rx) = oneshot::channel::<()>();

    let srv = server::http2()
        .route("/", "hello retry")
        .delay_listen(rx.map(|_| ()))
        .await;
    let proxy = proxy::new().inbound(srv).run().await;
    let client = client::http2(proxy.inbound, "transparency.example.com");
    let metrics = client::http1(proxy.metrics, "localhost");

    let fut = client.request(client.request_builder("/").version(http::Version::HTTP_2));

    // wait until metrics has seen our connection, this can be flaky depending on
    // all the other threads currently running...
    metrics::metric("tcp_open_total")
        .label("peer", "src")
        .label("direction", "inbound")
        .label("srv_name", "default:all-unauthenticated")
        .value(1u64)
        .assert_in(&metrics)
        .await;

    drop(tx); // start `listen` now
              // This may be flaky on CI.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let res = fut.await.expect("response");
    assert_eq!(res.status(), http::StatusCode::OK);
}

#[tokio::test]
async fn http2_request_without_authority() {
    let _trace = trace_init();

    let srv = server::http2()
        .route_fn("/", |req| {
            assert_eq!(req.uri().authority(), None);
            Response::new("".into())
        })
        .run()
        .await;
    let proxy = proxy::new().inbound(srv).run().await;

    // Make a single HTTP/2 request without an :authority header.
    //
    // The support::client expects request URIs to be in absolute-form. It's
    // easier to customize this one case than to make the support::client more
    // complicated.
    let addr = proxy.inbound;
    let io = tokio::net::TcpStream::connect(&addr)
        .await
        .expect("connect error");
    let (mut client, conn) = hyper::client::conn::Builder::new()
        .http2_only(true)
        .handshake(io)
        .await
        .expect("handshake error");

    tokio::spawn(conn.map_err(|e| tracing::info!("conn error: {:?}", e)));

    let req = Request::new(hyper::Body::empty());
    // these properties are specifically what we want, and set by default
    assert_eq!(req.uri(), "/");
    assert_eq!(req.version(), http::Version::HTTP_11);
    let res = client.send_request(req).await.expect("client error");

    assert_eq!(res.status(), http::StatusCode::OK);

    proxy.join_servers().await;
}

#[tokio::test]
async fn http2_rst_stream_is_propagated() {
    let _trace = trace_init();

    let reason = h2::Reason::ENHANCE_YOUR_CALM;

    let srv = server::http2()
        .route_async("/", move |_req| async move { Err(h2::Error::from(reason)) })
        .run()
        .await;
    let proxy = proxy::new().inbound(srv).run().await;
    let client = client::http2(proxy.inbound, "transparency.example.com");

    let err: hyper::Error = client
        .request(client.request_builder("/"))
        .await
        .expect_err("client request should error");

    let rst = err
        .source()
        .expect("error should have a source")
        .downcast_ref::<h2::Error>()
        .expect("source should be h2::Error");

    assert_eq!(rst.reason(), Some(reason));
}

#[tokio::test]
async fn http1_orig_proto_does_not_propagate_rst_stream() {
    let _trace = trace_init();

    // Use a custom http2 server to "act" as an inbound proxy so we
    // can trigger a RST_STREAM.
    let srv = server::http2()
        .route_async("/", move |req| async move {
            assert!(req.headers().contains_key("l5d-orig-proto"));
            Err(h2::Error::from(h2::Reason::ENHANCE_YOUR_CALM))
        })
        .run()
        .await;
    let ctrl = controller::new();
    let host = "transparency.test.svc.cluster.local";
    let _profile = ctrl.profile_tx_default(srv.addr, host);
    let dst = ctrl.destination_tx(format!("{}:{}", host, srv.addr.port()));
    dst.send_h2_hinted(srv.addr);
    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .outbound(srv)
        .run()
        .await;
    let addr = proxy.outbound;

    let client = client::http1(addr, host);
    let res = client
        .request(client.request_builder("/"))
        .await
        .expect("client request");

    assert_eq!(res.status(), http::StatusCode::BAD_GATEWAY);

    proxy.join_servers().await;
}
