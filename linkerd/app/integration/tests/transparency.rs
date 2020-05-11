#![deny(warnings, rust_2018_idioms)]
#![type_length_limit = "1586225"]

use linkerd2_app_integration::*;
use std::error::Error as _;
use std::sync::mpsc;

#[test]
#[cfg_attr(not(feature = "nyi"), ignore)]
fn outbound_http1() {
    let _ = trace_init();

    let srv = server::http1().route("/", "hello h1").run();
    let ctrl = controller::new();
    ctrl.profile_tx_default("transparency.test.svc.cluster.local");
    ctrl.destination_tx("transparency.test.svc.cluster.local")
        .send_addr(srv.addr);
    let proxy = proxy::new().controller(ctrl.run()).outbound(srv).run();
    let client = client::http1(proxy.outbound, "transparency.test.svc.cluster.local");

    assert_eq!(client.get("/"), "hello h1");
}

#[test]
#[cfg_attr(not(feature = "nyi"), ignore)]
fn inbound_http1() {
    let _ = trace_init();

    let srv = server::http1().route("/", "hello h1").run();
    let ctrl = controller::new();
    ctrl.profile_tx_default("transparency.test.svc.cluster.local");
    let proxy = proxy::new()
        .controller(ctrl.run())
        .inbound_fuzz_addr(srv)
        .run();
    let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

    assert_eq!(client.get("/"), "hello h1");
}

#[test]
fn outbound_tcp() {
    let _ = trace_init();

    let msg1 = "custom tcp hello";
    let msg2 = "custom tcp bye";

    let srv = server::tcp()
        .accept(move |read| {
            assert_eq!(read, msg1.as_bytes());
            msg2
        })
        .run();
    let proxy = proxy::new().outbound(srv).run();

    let client = client::tcp(proxy.outbound);

    let tcp_client = client.connect();

    tcp_client.write(msg1);
    assert_eq!(tcp_client.read(), msg2.as_bytes());
}

#[test]
fn inbound_tcp() {
    let _ = trace_init();

    let msg1 = "custom tcp hello";
    let msg2 = "custom tcp bye";

    let srv = server::tcp()
        .accept(move |read| {
            assert_eq!(read, msg1.as_bytes());
            msg2
        })
        .run();
    let proxy = proxy::new().inbound_fuzz_addr(srv).run();

    let client = client::tcp(proxy.inbound);

    let tcp_client = client.connect();

    tcp_client.write(msg1);
    assert_eq!(tcp_client.read(), msg2.as_bytes());
}

fn test_server_speaks_first(env: TestEnv) {
    const TIMEOUT: Duration = Duration::from_secs(5);

    let _ = trace_init();

    let msg1 = "custom tcp server starts";
    let msg2 = "custom tcp client second";

    let (tx, rx) = mpsc::channel();
    let srv = server::tcp()
        .accept_fut(move |mut sock| {
            async move {
                sock.write_all(msg1.as_bytes()).await?;
                let mut vec = vec![0; 512];
                let n = sock.read_to_end(&mut vec).await?;
                assert_eq!(s(&vec[..n]), msg2);
                tx.send(()).unwrap();
                Ok(())
            }
            .map(|res: std::io::Result<()>| match res {
                Err(e) => panic!("tcp server error: {}", e),
                Ok(()) => {}
            })
        })
        .run();

    let proxy = proxy::new()
        .disable_inbound_ports_protocol_detection(vec![srv.addr.port()])
        .inbound(srv)
        .run_with_test_env(env);

    let client = client::tcp(proxy.inbound);

    let tcp_client = client.connect();

    assert_eq!(s(&tcp_client.read_timeout(TIMEOUT)), msg1);
    tcp_client.write(msg2);
    rx.recv_timeout(TIMEOUT).unwrap();
}

#[test]
fn tcp_server_first() {
    test_server_speaks_first(TestEnv::new());
}

#[test]
fn tcp_server_first_tls() {
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

    let env = TestEnv::new();

    // FIXME
    //env.put(app::env::ENV_TLS_CERT, cert);
    //env.put(app::env::ENV_TLS_PRIVATE_KEY, key);
    //env.put(app::env::ENV_TLS_TRUST_ANCHORS, trust_anchors);
    //env.put(
    //    app::env::ENV_TLS_LOCAL_IDENTITY,
    //    "foo.deployment.ns1.linkerd-managed.linkerd.svc.cluster.local".to_string(),
    //);

    test_server_speaks_first(env)
}

#[test]
fn tcp_connections_close_if_client_closes() {
    use std::sync::mpsc;

    let _ = trace_init();

    let msg1 = "custom tcp hello";
    let msg2 = "custom tcp bye";

    let (tx, rx) = mpsc::channel();

    let srv = server::tcp()
        .accept_fut(move |mut sock| {
            async move {
                let mut vec = vec![0; 1024];
                let n = sock.read(&mut vec).await?;

                assert_eq!(s(&vec[..n]), msg1);
                sock.write_all(msg2.as_bytes()).await?;
                let n = sock.read(&mut [0; 16]).await?;
                assert_eq!(n, 0);
                tx.send(()).unwrap();
                Ok(())
            }
            .map(|res: std::io::Result<()>| match res {
                Err(e) => panic!("tcp server error: {}", e),
                Ok(()) => {}
            })
        })
        .run();
    let proxy = proxy::new().inbound(srv).run();

    let client = client::tcp(proxy.inbound);

    let tcp_client = client.connect();
    tcp_client.write(msg1);
    assert_eq!(s(&tcp_client.read()[..]), msg2);

    drop(tcp_client);

    // rx will be fulfilled when our tcp accept_fut sees
    // a socket disconnect, which is what we are testing for.
    // the timeout here is just to prevent this test from hanging
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
}

macro_rules! http1_tests {
    (proxy: $proxy:expr) => {
        #[test]
        #[cfg_attr(not(feature = "nyi"), ignore)]
        fn inbound_http1() {
            let _ = trace_init();

            let srv = server::http1().route("/", "hello h1").run();
            let proxy = $proxy(srv);
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            assert_eq!(client.get("/"), "hello h1");
        }

        #[test]
        fn http1_removes_connection_headers() {
            let _ = trace_init();

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
                .run();
            let proxy = $proxy(srv);
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let res = client.request(
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
            );

            assert_eq!(res.status(), http::StatusCode::OK);
            // assert!(!res.headers().contains_key("x-server-quux"));
        }

        #[test]
        fn http10_with_host() {
            let _ = trace_init();

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
                .run();
            let proxy = $proxy(srv);
            let client = client::http1(proxy.inbound, host);

            let res = client.request(
                client
                    .request_builder("/")
                    .version(http::Version::HTTP_10)
                    .header("host", host),
            );

            assert_eq!(res.status(), http::StatusCode::OK);
            assert_eq!(res.version(), http::Version::HTTP_10);
        }

        #[test]
        fn http11_absolute_uri_differs_from_host() {
            let _ = trace_init();

            // We shouldn't touch the URI or the Host, just pass directly as we got.
            let auth = "transparency.test.svc.cluster.local";
            let host = "foo.bar";
            let srv = server::http1()
                .route_fn("/", move |req| {
                    assert_eq!(req.headers()["host"], host);
                    assert_eq!(req.uri().to_string(), format!("http://{}/", auth));
                    Response::new("".into())
                })
                .run();
            let proxy = $proxy(srv);
            let client = client::http1_absolute_uris(proxy.inbound, auth);

            let res = client.request(
                client
                    .request_builder("/")
                    .version(http::Version::HTTP_11)
                    .header("host", host),
            );

            assert_eq!(res.status(), http::StatusCode::OK);
            assert_eq!(res.version(), http::Version::HTTP_11);
        }

        #[test]
        #[cfg_attr(not(feature = "nyi"), ignore)]
        fn http11_upgrades() {
            let _ = trace_init();

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
                        vec.clear();
                        let n = sock.read(&mut vec).await?;
                        assert_eq!(s(&vec[..n]), chatproto_req);

                        // Some processing... and then write back in chatproto...
                        sock.write_all(chatproto_res.as_bytes()).await
                    }
                    .map(|res: std::io::Result<()>| match res {
                        Ok(()) => {}
                        Err(e) => panic!("tcp server error: {}", e),
                    })
                })
                .run();
            let proxy = $proxy(srv);

            let client = client::tcp(proxy.inbound);

            let tcp_client = client.connect();

            tcp_client.write(upgrade_req);

            let resp = tcp_client.read();
            let resp_str = s(&resp);
            assert!(
                resp_str.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
                "response not an upgrade: {:?}",
                resp_str
            );
            assert_contains!(resp_str, upgrade_needle);

            // We've upgraded from HTTP to chatproto! Say hi!
            tcp_client.write(chatproto_req);
            // Did anyone respond?
            let chat_resp = tcp_client.read();
            assert_eq!(s(&chat_resp), chatproto_res);
        }

        #[test]
        fn l5d_orig_proto_header_isnt_leaked() {
            let _ = trace_init();

            let srv = server::http1()
                .route_fn("/", |req| {
                    assert_eq!(req.headers().get("l5d-orig-proto"), None, "request");
                    Response::new(Default::default())
                })
                .run();
            let proxy = $proxy(srv);

            let host = "transparency.test.svc.cluster.local";
            let client = client::http1(proxy.inbound, host);

            let res = client.request(client.request_builder("/"));
            assert_eq!(res.status(), 200);
            assert_eq!(res.headers().get("l5d-orig-proto"), None, "response");
        }

        #[test]
        fn http11_upgrade_h2_stripped() {
            let _ = trace_init();

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
                .run();
            let proxy = $proxy(srv);

            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let res = client.request(
                client
                    .request_builder("/")
                    .header("upgrade", "h2c")
                    .header("http2-settings", "")
                    .header("connection", "upgrade, http2-settings"),
            );

            // If the assertion is trigger in the above test route, the proxy will
            // just send back a 500.
            assert_eq!(res.status(), http::StatusCode::OK);
        }

        #[test]
        #[cfg_attr(not(feature = "nyi"), ignore)]
        fn http11_connect() {
            let _ = trace_init();

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
                        vec.clear();
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
                .run();
            let proxy = $proxy(srv);

            let client = client::tcp(proxy.inbound);

            let tcp_client = client.connect();

            tcp_client.write(&connect_req[..]);

            let resp = tcp_client.read();
            let resp_str = s(&resp);
            assert!(
                resp_str.starts_with("HTTP/1.1 200 OK\r\n"),
                "response not an upgrade: {:?}",
                resp_str
            );

            // We've CONNECTed from HTTP to foo.bar! Say hi!
            tcp_client.write(&tunneled_req[..]);
            // Did anyone respond?
            let resp2 = tcp_client.read();
            assert_eq!(s(&resp2), s(&tunneled_res[..]));
        }

        #[test]
        #[cfg_attr(not(feature = "nyi"), ignore)]
        fn http11_connect_bad_requests() {
            let _ = trace_init();

            let srv = server::tcp()
                .accept(move |_sock| -> Vec<u8> {
                    unreachable!("shouldn't get through the proxy");
                })
                .run();
            let proxy = $proxy(srv);

            // A TCP client is used since the HTTP client would stop these requests
            // from ever touching the network.
            let client = client::tcp(proxy.inbound);

            let bad_uris = vec!["/origin-form", "/", "http://test/bar", "http://test", "*"];

            for bad_uri in bad_uris {
                let tcp_client = client.connect();

                let req = format!("CONNECT {} HTTP/1.1\r\nHost: test\r\n\r\n", bad_uri);
                tcp_client.write(req);

                let resp = tcp_client.read();
                let resp_str = s(&resp);
                assert!(
                    resp_str.starts_with("HTTP/1.1 400 Bad Request\r\n"),
                    "bad URI ({:?}) should get 400 response: {:?}",
                    bad_uri,
                    resp_str
                );
            }

            // origin-form URIs must be CONNECT
            let tcp_client = client.connect();

            tcp_client.write("GET test HTTP/1.1\r\nHost: test\r\n\r\n");

            let resp = tcp_client.read();
            let resp_str = s(&resp);
            assert!(
                resp_str.starts_with("HTTP/1.1 400 Bad Request\r\n"),
                "origin-form without CONNECT should get 400 response: {:?}",
                resp_str
            );

            // check that HTTP/1.0 is not allowed for CONNECT
            let tcp_client = client.connect();

            tcp_client.write("CONNECT test HTTP/1.0\r\nHost: test\r\n\r\n");

            let resp = tcp_client.read();
            let resp_str = s(&resp);
            assert!(
                resp_str.starts_with("HTTP/1.0 400 Bad Request\r\n"),
                "HTTP/1.0 CONNECT should get 400 response: {:?}",
                resp_str
            );
        }

        #[tokio::test]
        async fn http1_request_with_body_content_length() {
            let _ = trace_init();

            let srv = server::http1()
                .route_fn("/", |req| {
                    assert_eq!(req.headers()["content-length"], "5");
                    Response::default()
                })
                .run();
            let proxy = $proxy(srv);
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let req = client
                .request_builder("/")
                .method("POST")
                .body("hello".into())
                .unwrap();

            let resp = client.request_body_async(req).await;

            assert_eq!(resp.status(), StatusCode::OK);
        }

        #[tokio::test]
        #[cfg_attr(not(feature = "nyi"), ignore)]
        async fn http1_request_with_body_chunked() {
            let _ = trace_init();

            let srv = server::http1()
                .route_async("/", |req| async move {
                    assert_eq!(req.headers()["transfer-encoding"], "chunked");
                    let body = req
                        .into_body()
                        .fold(String::new(), |s, mut chunk| {
                            s + std::str::from_utf8(chunk.to_bytes().as_ref()).expect("req is utf8")
                        })
                        .await;
                    assert_eq!(body, "hello");
                    Ok::<_, std::io::Error>(
                        Response::builder()
                            .header("transfer-encoding", "chunked")
                            .body("world".into())
                            .unwrap(),
                    )
                })
                .run();
            let proxy = $proxy(srv);
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let req = client
                .request_builder("/")
                .method("POST")
                .header("transfer-encoding", "chunked")
                .body("hello".into())
                .unwrap();

            let resp = client.request_body_async(req).await;

            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(resp.headers()["transfer-encoding"], "chunked");
            let mut body = hyper::body::aggregate(resp.into_body())
                .await
                .expect("rsp aggregate");
            let body = std::str::from_utf8(body.to_bytes().as_ref())
                .expect("rsp is utf8")
                .to_owned();
            assert_eq!(body, "world");
        }

        #[test]
        #[cfg_attr(not(feature = "nyi"), ignore)]
        fn http1_requests_without_body_doesnt_add_transfer_encoding() {
            let _ = trace_init();

            let srv = server::http1()
                .route_fn("/", |req| {
                    let has_body_header = req.headers().contains_key("transfer-encoding")
                        || req.headers().contains_key("content-length");
                    let status = if has_body_header {
                        StatusCode::BAD_REQUEST
                    } else {
                        StatusCode::OK
                    };
                    let mut res = Response::new("".into());
                    *res.status_mut() = status;
                    res
                })
                .run();
            let proxy = $proxy(srv);
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let methods = &["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];

            for &method in methods {
                let resp = client.request(client.request_builder("/").method(method));

                assert_eq!(resp.status(), StatusCode::OK, "method={:?}", method);
            }
        }

        #[test]
        fn http1_content_length_zero_is_preserved() {
            let _ = trace_init();

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
                .run();
            let proxy = $proxy(srv);
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let methods = &["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];

            for &method in methods {
                let resp = client.request(
                    client
                        .request_builder("/")
                        .method(method)
                        .header("content-length", "0"),
                );

                assert_eq!(resp.status(), StatusCode::OK, "method={:?}", method);
                assert_eq!(resp.headers()["content-length"], "0", "method={:?}", method);
            }
        }

        #[test]
        #[cfg_attr(not(feature = "nyi"), ignore)]
        fn http1_bodyless_responses() {
            let _ = trace_init();

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
                .run();
            let proxy = $proxy(srv);
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            // https://tools.ietf.org/html/rfc7230#section-3.3.3
            // > response to a HEAD request, any 1xx, 204, or 304 cannot contain a body

            //TODO: the proxy doesn't support CONNECT requests yet, but when we do,
            //they should be tested here as well. As RFC7230 says, a 2xx response to
            //a CONNECT request is not allowed to contain a body (but 4xx, 5xx can!).

            let resp = client.request(client.request_builder("/").method("HEAD"));

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
                let resp = client.request(
                    client
                        .request_builder("/")
                        .header(req_status_header, status.as_str()),
                );

                assert_eq!(resp.status(), status);
                assert!(
                    !resp.headers().contains_key("transfer-encoding"),
                    "transfer-encoding with status={:?}",
                    status
                );
            }
        }

        #[tokio::test]
        #[cfg_attr(not(feature = "nyi"), ignore)]
        async fn http1_head_responses() {
            let _ = trace_init();

            let srv = server::http1()
                .route_fn("/", move |req| {
                    assert_eq!(req.method(), "HEAD");
                    Response::builder()
                        .header("content-length", "55")
                        .body("".into())
                        .unwrap()
                })
                .run();
            let proxy = $proxy(srv);
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let resp = client
                .request_async(client.request_builder("/").method("HEAD"))
                .await
                .expect("request");

            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(resp.headers()["content-length"], "55");

            let mut body = hyper::body::aggregate(resp.into_body())
                .await
                .expect("response body aggregate");
            let body = std::str::from_utf8(body.to_bytes().as_ref())
                .expect("empty body is utf8")
                .to_owned();

            assert_eq!(body, "");
        }

        #[tokio::test]
        #[cfg_attr(not(feature = "nyi"), ignore)]
        async fn http1_response_end_of_file() {
            let _ = trace_init();

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
                .run();
            let proxy = $proxy(srv);

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
                    .request_async(client.request_builder("/").method("GET"))
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

                let mut body = hyper::body::aggregate(resp.into_body())
                    .await
                    .expect("aggregate response body");
                let body = std::str::from_utf8(body.to_bytes().as_ref())
                    .expect("body is utf8")
                    .to_owned();

                assert_eq!(body, "body till eof", "HTTP/{} body", v);
            }
        }
    };
}

mod one_proxy {
    use linkerd2_app_integration::*;

    http1_tests! { proxy: |srv| {
        let ctrl = controller::new();
        ctrl.profile_tx_default("transparency.test.svc.cluster.local");
        proxy::new().inbound(srv).controller(ctrl.run()).run()
    }}
}

mod proxy_to_proxy {
    use linkerd2_app_integration::*;

    struct ProxyToProxy {
        // Held to prevent closing, to reduce controller request noise during
        // tests
        _dst: controller::DstSender,
        _in: proxy::Listening,
        _out: proxy::Listening,
        inbound: SocketAddr,
    }

    http1_tests! { proxy: |srv| {
        let ctrl = controller::new();
        ctrl.profile_tx_default("transparency.test.svc.cluster.local");
        let inbound = proxy::new().controller(ctrl.run()).inbound(srv).run();

        let ctrl = controller::new();
        ctrl.profile_tx_default("transparency.test.svc.cluster.local");
        let dst = ctrl.destination_tx("transparency.test.svc.cluster.local");
        dst.send_h2_hinted(inbound.inbound);

        let outbound = proxy::new()
            .controller(ctrl.run())
            .run();

        let addr = outbound.outbound;
        ProxyToProxy {
            _dst: dst,
            _in: inbound,
            _out: outbound,
            inbound: addr,
        }
    } }
}

#[test]
fn http10_without_host() {
    // Without a host or authority, there's no way to route this test,
    // so its not part of the proxy_to_proxy set.
    let _ = trace_init();

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
        .run();
    let proxy = proxy::new().inbound(srv).run();

    let client = client::tcp(proxy.inbound);

    let tcp_client = client.connect();

    tcp_client.write(
        "\
         GET / HTTP/1.0\r\n\
         \r\n\
         ",
    );

    let expected = "HTTP/1.0 200 OK\r\n";
    assert_eq!(s(&tcp_client.read()[..expected.len()]), expected);
}

#[test]
#[cfg_attr(not(feature = "nyi"), ignore)]
fn http1_one_connection_per_host() {
    let _ = trace_init();

    let srv = server::http1()
        .route("/body", "hello hosts")
        .route("/no-body", "")
        .run();
    let proxy = proxy::new().inbound(srv).run();

    let client = client::http1(proxy.inbound, "foo.bar");

    let inbound = &proxy.inbound_server.as_ref().expect("no inbound server!");

    // Run each case with and without a body.
    let run_request = move |host, expected_conn_cnt| {
        for path in &["/no-body", "/body"][..] {
            let res = client.request(
                client
                    .request_builder(path)
                    .version(http::Version::HTTP_11)
                    .header("host", host),
            );
            assert_eq!(res.status(), http::StatusCode::OK);
            assert_eq!(res.version(), http::Version::HTTP_11);
            assert_eq!(inbound.connections(), expected_conn_cnt);
        }
    };

    // Make a request with the header "Host: foo.bar". After the request, the
    // server should have seen one connection.
    run_request("foo.bar", 1);

    // Another request with the same host. The proxy may reuse the connection.
    run_request("foo.bar", 1);

    // Make a request with a different Host header. This request must use a new
    // connection.
    run_request("bar.baz", 2);
    run_request("bar.baz", 2);

    // Make a request with a different Host header. This request must use a new
    // connection.
    run_request("quuuux.com", 3);
}

#[test]
#[cfg_attr(not(feature = "nyi"), ignore)]
fn http1_requests_without_host_have_unique_connections() {
    let _ = trace_init();

    let srv = server::http1().route("/", "unique hosts").run();
    let proxy = proxy::new().inbound(srv).run();

    let client = client::http1(proxy.inbound, "foo.bar");

    let inbound = &proxy.inbound_server.as_ref().expect("no inbound server!");

    // Make a request with no Host header and no authority in the request path.
    let res = client.request(
        client
            .request_builder("/")
            .version(http::Version::HTTP_11)
            .header("host", ""),
    );
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.version(), http::Version::HTTP_11);
    assert_eq!(inbound.connections(), 1);

    // Another request with no Host. The proxy must open a new connection
    // for that request.
    let res = client.request(
        client
            .request_builder("/")
            .version(http::Version::HTTP_11)
            .header("host", ""),
    );
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.version(), http::Version::HTTP_11);
    assert_eq!(inbound.connections(), 2);

    // Make a request with a host header. It must also receive its
    // own connection.
    let res = client.request(
        client
            .request_builder("/")
            .version(http::Version::HTTP_11)
            .header("host", "foo.bar"),
    );
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.version(), http::Version::HTTP_11);
    assert_eq!(inbound.connections(), 3);

    // Another request with no Host. The proxy must open a new connection
    // for that request.
    let res = client.request(
        client
            .request_builder("/")
            .version(http::Version::HTTP_11)
            .header("host", ""),
    );
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.version(), http::Version::HTTP_11);
    assert_eq!(inbound.connections(), 4);
}

#[tokio::test]
#[cfg_attr(not(feature = "nyi"), ignore)]
async fn retry_reconnect_errors() {
    let _ = trace_init();

    // Used to delay `listen` in the server, to force connection refused errors.
    let (tx, rx) = oneshot::channel::<()>();

    let srv = server::http2()
        .route("/", "hello retry")
        .delay_listen(rx.map(|_| ()));
    let proxy = proxy::new().inbound(srv).run();
    let client = client::http2(proxy.inbound, "transparency.test.svc.cluster.local");
    let metrics = client::http1(proxy.metrics, "localhost");

    let fut = client.request_async(client.request_builder("/").version(http::Version::HTTP_2));

    // wait until metrics has seen our connection, this can be flaky depending on
    // all the other threads currently running...
    assert_eventually_contains!(
        metrics.get("/metrics"),
        "tcp_open_total{direction=\"inbound\",peer=\"src\",tls=\"disabled\"} 1"
    );

    drop(tx); // start `listen` now
    let res = fut.await.expect("response");
    assert_eq!(res.status(), http::StatusCode::OK);
}

#[tokio::test]
async fn http2_request_without_authority() {
    let _ = trace_init();

    let srv = server::http2()
        .route_fn("/", |req| {
            assert_eq!(req.uri().authority(), None);
            Response::new("".into())
        })
        .run();
    let proxy = proxy::new().inbound_fuzz_addr(srv).run();

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

    tokio::spawn(conn.map_err(|e| println!("conn error: {:?}", e)));

    let req = Request::new(hyper::Body::empty());
    // these properties are specifically what we want, and set by default
    assert_eq!(req.uri(), "/");
    assert_eq!(req.version(), http::Version::HTTP_11);
    let res = client.send_request(req).await.expect("client error");

    assert_eq!(res.status(), http::StatusCode::OK);
}

#[tokio::test]
#[cfg_attr(not(feature = "nyi"), ignore)]
async fn http2_rst_stream_is_propagated() {
    let _ = trace_init();

    let reason = h2::Reason::ENHANCE_YOUR_CALM;

    let srv = server::http2()
        .route_async("/", move |_req| async move { Err(h2::Error::from(reason)) })
        .run();
    let proxy = proxy::new().inbound_fuzz_addr(srv).run();
    let client = client::http2(proxy.inbound, "transparency.test.svc.cluster.local");

    let err: hyper::Error = client
        .request_async(client.request_builder("/"))
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
#[cfg_attr(not(feature = "nyi"), ignore)]
async fn http1_orig_proto_does_not_propagate_rst_stream() {
    let _ = trace_init();

    // Use a custom http2 server to "act" as an inbound proxy so we
    // can trigger a RST_STREAM.
    let srv = server::http2()
        .route_async("/", move |req| async move {
            assert!(req.headers().contains_key("l5d-orig-proto"));
            Err(h2::Error::from(h2::Reason::ENHANCE_YOUR_CALM))
        })
        .run();
    let ctrl = controller::new();
    let host = "transparency.test.svc.cluster.local";
    let dst = ctrl.destination_tx(host);
    dst.send_h2_hinted(srv.addr);
    let proxy = proxy::new().controller(ctrl.run()).run();
    let addr = proxy.outbound;

    let client = client::http1(addr, host);
    let res = client
        .request_async(client.request_builder("/"))
        .await
        .expect("client request");

    assert_eq!(res.status(), http::StatusCode::BAD_GATEWAY);
}
