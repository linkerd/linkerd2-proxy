#![deny(warnings)]
#[macro_use]
mod support;
use self::support::*;

#[test]
fn outbound_http1() {
    let _ = env_logger::try_init();

    let srv = server::http1().route("/", "hello h1").run();
    let ctrl = controller::new()
        .destination_and_close("transparency.test.svc.cluster.local", srv.addr)
        .run();
    let proxy = proxy::new().controller(ctrl).outbound(srv).run();
    let client = client::http1(proxy.outbound, "transparency.test.svc.cluster.local");

    assert_eq!(client.get("/"), "hello h1");
}

#[test]
fn inbound_http1() {
    let _ = env_logger::try_init();

    let srv = server::http1().route("/", "hello h1").run();
    let proxy = proxy::new()
        .inbound_fuzz_addr(srv)
        .run();
    let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

    assert_eq!(client.get("/"), "hello h1");
}

#[test]
fn outbound_tcp() {
    let _ = env_logger::try_init();

    let msg1 = "custom tcp hello";
    let msg2 = "custom tcp bye";

    let srv = server::tcp()
        .accept(move |read| {
            assert_eq!(read, msg1.as_bytes());
            msg2
        })
        .run();
    let proxy = proxy::new()
        .outbound(srv)
        .run();

    let client = client::tcp(proxy.outbound);

    let tcp_client = client.connect();

    tcp_client.write(msg1);
    assert_eq!(tcp_client.read(), msg2.as_bytes());
}

#[test]
fn inbound_tcp() {
    let _ = env_logger::try_init();

    let msg1 = "custom tcp hello";
    let msg2 = "custom tcp bye";

    let srv = server::tcp()
        .accept(move |read| {
            assert_eq!(read, msg1.as_bytes());
            msg2
        })
        .run();
    let proxy = proxy::new()
        .inbound_fuzz_addr(srv)
        .run();

    let client = client::tcp(proxy.inbound);

    let tcp_client = client.connect();

    tcp_client.write(msg1);
    assert_eq!(tcp_client.read(), msg2.as_bytes());
}

#[test]
fn tcp_server_first() {
    use std::sync::mpsc;

    let _ = env_logger::try_init();

    let msg1 = "custom tcp server starts";
    let msg2 = "custom tcp client second";

    let (tx, rx) = mpsc::channel();

    let srv = server::tcp()
        .accept_fut(move |sock| {
            tokio_io::io::write_all(sock, msg1.as_bytes())
                .and_then(move |(sock, _)| {
                    tokio_io::io::read(sock, vec![0; 512])
                })
                .map(move |(_sock, vec, n)| {
                    assert_eq!(&vec[..n], msg2.as_bytes());
                    tx.send(()).unwrap();
                })
                .map_err(|e| panic!("tcp server error: {}", e))
        })
        .run();
    let proxy = proxy::new()
        .disable_inbound_ports_protocol_detection(vec![srv.addr.port()])
        .inbound(srv)
        .run();

    let client = client::tcp(proxy.inbound);

    let tcp_client = client.connect();

    assert_eq!(tcp_client.read(), msg1.as_bytes());
    tcp_client.write(msg2);
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
}

#[test]
fn tcp_with_no_orig_dst() {
    let _ = env_logger::try_init();

    let srv = server::tcp()
        .accept(move |_| "don't read me")
        .run();
    let proxy = proxy::new()
        .inbound(srv)
        .run();

    // no outbound configured for proxy
    let client = client::tcp(proxy.outbound);

    let tcp_client = client.connect();
    tcp_client.write("custom tcp hello");

    let read = tcp_client
        .try_read()
        // This read might be an error, or an empty vec
        .unwrap_or_else(|_| Vec::new());
    assert_eq!(read, b"");
}

#[test]
fn tcp_connections_close_if_client_closes() {
    use std::sync::mpsc;

    let _ = env_logger::try_init();

    let msg1 = "custom tcp hello";
    let msg2 = "custom tcp bye";

    let (tx, rx) = mpsc::channel();

    let srv = server::tcp()
        .accept_fut(move |sock| {
            tokio_io::io::read(sock, vec![0; 1024])
                .and_then(move |(sock, vec, n)| {
                    assert_eq!(&vec[..n], msg1.as_bytes());

                    tokio_io::io::write_all(sock, msg2.as_bytes())
                }).and_then(|(sock, _)| {
                    // lets read again, but we should get eof
                    tokio_io::io::read(sock, [0; 16])
                })
                .map(move |(_sock, _vec, n)| {
                    assert_eq!(n, 0);
                    tx.send(()).unwrap();
                })
                .map_err(|e| panic!("tcp server error: {}", e))
        })
        .run();
    let proxy = proxy::new()
        .inbound(srv)
        .run();

    let client = client::tcp(proxy.inbound);

    let tcp_client = client.connect();
    tcp_client.write(msg1);
    assert_eq!(tcp_client.read(), msg2.as_bytes());

    drop(tcp_client);

    // rx will be fulfilled when our tcp accept_fut sees
    // a socket disconnect, which is what we are testing for.
    // the timeout here is just to prevent this test from hanging
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
}

macro_rules! http1_tests {
    (proxy: $proxy:expr) => {
        #[test]
        fn inbound_http1() {
            let _ = env_logger::try_init();

            let srv = server::http1().route("/", "hello h1").run();
            let proxy = $proxy(srv);
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            assert_eq!(client.get("/"), "hello h1");
        }

        #[test]
        fn http1_removes_connection_headers() {
            let _ = env_logger::try_init();

            let srv = server::http1()
                .route_fn("/", |req| {
                    assert!(!req.headers().contains_key("x-foo-bar"));
                    Response::builder()
                        .header("x-server-quux", "lorem ipsum")
                        .header("connection", "close, x-server-quux")
                        .header("keep-alive", "500")
                        .header("proxy-connection", "a")
                        .body("".into())
                        .unwrap()
                })
                .run();
            let proxy = $proxy(srv);
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            let res = client.request(client.request_builder("/")
                .header("x-foo-bar", "baz")
                .header("connection", "x-foo-bar, close")
                // These headers will fail in the proxy_to_proxy case if
                // they are not stripped.
                //
                // normally would be stripped by `connection: keep-alive`,
                // but test its removed even if the connection header forgot
                // about it.
                .header("keep-alive", "500")
                .header("proxy-connection", "a")
            );

            assert_eq!(res.status(), http::StatusCode::OK);
            assert!(!res.headers().contains_key("x-server-quux"));
        }


        #[test]
        fn http10_with_host() {
            let _ = env_logger::try_init();

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

            let res = client.request(client.request_builder("/")
                .version(http::Version::HTTP_10)
                .header("host", host));

            assert_eq!(res.status(), http::StatusCode::OK);
            assert_eq!(res.version(), http::Version::HTTP_10);
        }

        #[test]
        fn http11_absolute_uri_differs_from_host() {
            let _ = env_logger::try_init();

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

            let res = client.request(client.request_builder("/")
                .version(http::Version::HTTP_11)
                .header("host", host));

            assert_eq!(res.status(), http::StatusCode::OK);
            assert_eq!(res.version(), http::Version::HTTP_11);
        }

        #[test]
        fn http11_upgrades() {
            let _ = env_logger::try_init();

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
                .accept_fut(move |sock| {
                    // Read upgrade_req...
                    tokio_io::io::read(sock, vec![0; 512])
                        .and_then(move |(sock, vec, n)| {
                            let head = s(&vec[..n]);
                            assert_contains!(head, upgrade_needle);

                            // Write upgrade_res back...
                            tokio_io::io::write_all(sock, upgrade_res)
                        })
                        .and_then(move |(sock, _)| {
                            // Read the message in 'chatproto' format
                            tokio_io::io::read(sock, vec![0; 512])
                        })
                        .and_then(move |(sock, vec, n)| {
                            assert_eq!(s(&vec[..n]), chatproto_req);

                            // Some processing... and then write back in chatproto...
                            tokio_io::io::write_all(sock, chatproto_res)
                        })
                        .map(|_| ())
                        .map_err(|e| panic!("tcp server error: {}", e))
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
        fn http11_upgrade_h2_stripped() {
            let _ = env_logger::try_init();

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

            let res = client.request(client.request_builder("/")
                .header("upgrade", "h2c")
                .header("http2-settings", "")
                .header("connection", "upgrade, http2-settings"));

            // If the assertion is trigger in the above test route, the proxy will
            // just send back a 500.
            assert_eq!(res.status(), http::StatusCode::OK);
        }

        #[test]
        fn http11_connect() {
            let _ = env_logger::try_init();

            // To simplify things for this test, we just use the test TCP
            // client and server to do an HTTP CONNECT.
            //
            // We don't *actually* perfom a new connect to requested host,
            // but client doesn't need to know that for our tests.

            let connect_req = "\
                CONNECT transparency.test.svc.cluster.local HTTP/1.1\r\n\
                Host: transparency.test.svc.cluster.local\r\n\
                \r\n\
                ";
            let connect_res = "\
                HTTP/1.1 200 OK\r\n\
                \r\n\
                ";

            let tunneled_req = "{send}: hi all\n";
            let tunneled_res = "{recv}: welcome!\n";

            let srv = server::tcp()
                .accept_fut(move |sock| {
                    // Read connect_req...
                    tokio_io::io::read(sock, vec![0; 512])
                        .and_then(move |(sock, vec, n)| {
                            let head = s(&vec[..n]);
                            assert_contains!(head, "CONNECT transparency.test.svc.cluster.local HTTP/1.1\r\n");

                            // Write connect_res back...
                            tokio_io::io::write_all(sock, connect_res)
                        })
                        .and_then(move |(sock, _)| {
                            // Read the message after tunneling...
                            tokio_io::io::read(sock, vec![0; 512])
                        })
                        .and_then(move |(sock, vec, n)| {
                            assert_eq!(s(&vec[..n]), tunneled_req);

                            // Some processing... and then write back tunneled res...
                            tokio_io::io::write_all(sock, tunneled_res)
                        })
                        .map(|_| ())
                        .map_err(|e| panic!("tcp server error: {}", e))
                })
                .run();
            let proxy = $proxy(srv);

            let client = client::tcp(proxy.inbound);

            let tcp_client = client.connect();

            tcp_client.write(connect_req);

            let resp = tcp_client.read();
            let resp_str = s(&resp);
            assert!(
                resp_str.starts_with("HTTP/1.1 200 OK\r\n"),
                "response not an upgrade: {:?}",
                resp_str
            );

            // We've CONNECTed from HTTP to foo.bar! Say hi!
            tcp_client.write(tunneled_req);
            // Did anyone respond?
            let resp2 = tcp_client.read();
            assert_eq!(s(&resp2), tunneled_res);
        }

        #[test]
        fn http11_connect_bad_requests() {
            let _ = env_logger::try_init();

            let srv = server::tcp()
                .accept(move |_sock| -> Vec<u8> {
                    unreachable!("shouldn't get through the proxy");
                })
                .run();
            let proxy = $proxy(srv);

            // A TCP client is used since the HTTP client would stop these requests
            // from ever touching the network.
            let client = client::tcp(proxy.inbound);

            let bad_uris = vec![
                "/origin-form",
                "/",
                "http://test/bar",
                "http://test",
                "*",
            ];

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

        #[test]
        fn http1_request_with_body_content_length() {
            let _ = env_logger::try_init();

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

            let resp = client.request_body(req);

            assert_eq!(resp.status(), StatusCode::OK);
        }

        #[test]
        fn http1_request_with_body_chunked() {
            let _ = env_logger::try_init();

            let srv = server::http1()
                .route_async("/", |req| {
                    assert_eq!(req.headers()["transfer-encoding"], "chunked");
                    req
                        .into_body()
                        .concat2()
                        .map(|body| {
                            assert_eq!(body, "hello");
                            Response::builder()
                                .header("transfer-encoding", "chunked")
                                .body("world".into())
                                .unwrap()
                        })
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

            let resp = client.request_body(req);

            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(resp.headers()["transfer-encoding"], "chunked");
            let body = resp.into_body().concat2().wait().unwrap();
            assert_eq!(body, "world");
        }

        #[test]
        fn http1_requests_without_body_doesnt_add_transfer_encoding() {
            let _ = env_logger::try_init();

            let srv = server::http1()
                .route_fn("/", |req| {
                    let has_body_header = req.headers().contains_key("transfer-encoding")
                        || req.headers().contains_key("content-length");
                    let status = if  has_body_header {
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

            let methods = &[
                "GET",
                "POST",
                "PUT",
                "DELETE",
                "HEAD",
                "PATCH",
            ];

            for &method in methods {
                let resp = client.request(
                    client
                        .request_builder("/")
                        .method(method)
                );

                assert_eq!(resp.status(), StatusCode::OK, "method={:?}", method);
            }
        }

        #[test]
        fn http1_content_length_zero_is_preserved() {
            let _ = env_logger::try_init();

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


            let methods = &[
                "GET",
                "POST",
                "PUT",
                "DELETE",
                "HEAD",
                "PATCH",
            ];

            for &method in methods {
                let resp = client.request(
                    client
                        .request_builder("/")
                        .method(method)
                        .header("content-length", "0")
                );

                assert_eq!(resp.status(), StatusCode::OK, "method={:?}", method);
                assert_eq!(resp.headers()["content-length"], "0", "method={:?}", method);
            }
        }

        #[test]
        fn http1_bodyless_responses() {
            let _ = env_logger::try_init();

            let req_status_header = "x-test-status-requested";

            let srv = server::http1()
                .route_fn("/", move |req| {
                    let status = req.headers()
                        .get(req_status_header)
                        .map(|val| {
                            val.to_str()
                                .expect("req_status_header should be ascii")
                                .parse::<u16>()
                                .expect("req_status_header should be numbers")
                        })
                        .unwrap_or(200);

                    Response::builder()
                        .status(status)
                        .body("".into())
                        .unwrap()
                })
                .run();
            let proxy = $proxy(srv);
            let client = client::http1(proxy.inbound, "transparency.test.svc.cluster.local");

            // https://tools.ietf.org/html/rfc7230#section-3.3.3
            // > response to a HEAD request, any 1xx, 204, or 304 cannot contain a body

            //TODO: the proxy doesn't support CONNECT requests yet, but when we do,
            //they should be tested here as well. As RFC7230 says, a 2xx response to
            //a CONNECT request is not allowed to contain a body (but 4xx, 5xx can!).

            let resp = client.request(
                client
                    .request_builder("/")
                    .method("HEAD")
            );

            assert_eq!(resp.status(), StatusCode::OK);
            assert!(!resp.headers().contains_key("transfer-encoding"));

            let statuses = &[
                //TODO: test some 1xx status codes.
                //The current test server doesn't support sending 1xx responses
                //easily. We could test this by making a new unit test with the
                //server being a TCP server, and write the response manually.
                StatusCode::NO_CONTENT, // 204
                StatusCode::NOT_MODIFIED, // 304
            ];

            for &status in statuses {
                let resp = client.request(
                    client
                        .request_builder("/")
                        .header(req_status_header, status.as_str())
                );

                assert_eq!(resp.status(), status);
                assert!(!resp.headers().contains_key("transfer-encoding"), "transfer-encoding with status={:?}", status);
            }
        }

        #[test]
        fn http1_head_responses() {
            let _ = env_logger::try_init();

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

            let resp = client.request(
                client
                    .request_builder("/")
                    .method("HEAD")
            );

            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(resp.headers()["content-length"], "55");

            let body = resp.into_body()
                .concat2()
                .wait()
                .expect("response body concat");

            assert_eq!(body, "");
        }

        #[test]
        fn http1_response_end_of_file() {
            let _ = env_logger::try_init();

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
                let resp = client.request(
                    client
                        .request_builder("/")
                        .method("GET")
                );

                assert_eq!(resp.status(), StatusCode::OK, "HTTP/{}", v);
                assert!(!resp.headers().contains_key("transfer-encoding"), "HTTP/{} transfer-encoding", v);
                assert!(!resp.headers().contains_key("content-length"), "HTTP/{} content-length", v);

                let body = resp.into_body()
                    .concat2()
                    .wait()
                    .expect("response body concat");

                assert_eq!(body, "body till eof", "HTTP/{} body", v);
            }
        }

    }
}

mod one_proxy {
    use super::support::*;

    http1_tests! { proxy: |srv| proxy::new().inbound(srv).run() }
}

mod proxy_to_proxy {
    use super::support::*;

    struct ProxyToProxy {
        // Held to prevent closing, to reduce controller request noise during
        // tests
        _dst: controller::DstSender,
        _in: proxy::Listening,
        _out: proxy::Listening,
        inbound: SocketAddr,
    }

    http1_tests! { proxy: |srv| {
        let inbound = proxy::new().inbound(srv).run();

        let ctrl = controller::new();
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
    let _ = env_logger::try_init();

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
    let proxy = proxy::new()
        .inbound(srv)
        .run();

    let client = client::tcp(proxy.inbound);

    let tcp_client = client.connect();

    tcp_client.write("\
        GET / HTTP/1.0\r\n\
        \r\n\
    ");

    let expected = "HTTP/1.0 200 OK\r\n";
    assert_eq!(s(&tcp_client.read()[..expected.len()]), expected);
}


#[test]
fn http1_one_connection_per_host() {
    let _ = env_logger::try_init();

    let srv = server::http1()
        .route("/body", "hello hosts")
        .route("/no-body", "")
        .run();
    let proxy = proxy::new().inbound(srv).run();

    let client = client::http1(proxy.inbound, "foo.bar");

    let inbound = &proxy.inbound_server.as_ref()
        .expect("no inbound server!");

    // Run each case with and without a body.
    let run_request = move |host, expected_conn_cnt| {
        for path in &["/no-body", "/body"][..] {
            let res = client.request(client.request_builder(path)
                .version(http::Version::HTTP_11)
                .header("host", host)
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
fn http1_requests_without_host_have_unique_connections() {
    let _ = env_logger::try_init();

    let srv = server::http1()
        .route("/", "unique hosts")
        .run();
    let proxy = proxy::new().inbound(srv).run();

    let client = client::http1(proxy.inbound, "foo.bar");

    let inbound = &proxy.inbound_server.as_ref()
        .expect("no inbound server!");

    // Make a request with no Host header and no authority in the request path.
    let res = client.request(client.request_builder("/")
        .version(http::Version::HTTP_11)
        .header("host", "")
    );
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.version(), http::Version::HTTP_11);
    assert_eq!(inbound.connections(), 1);

    // Another request with no Host. The proxy must open a new connection
    // for that request.
    let res = client.request(client.request_builder("/")
        .version(http::Version::HTTP_11)
        .header("host", "")
    );
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.version(), http::Version::HTTP_11);
    assert_eq!(inbound.connections(), 2);

    // Make a request with a host header. It must also receive its
    // own connection.
    let res = client.request(client.request_builder("/")
        .version(http::Version::HTTP_11)
        .header("host", "foo.bar")
    );
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.version(), http::Version::HTTP_11);
    assert_eq!(inbound.connections(), 3);

    // Another request with no Host. The proxy must open a new connection
    // for that request.
    let res = client.request(client.request_builder("/")
        .version(http::Version::HTTP_11)
        .header("host", "")
    );
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.version(), http::Version::HTTP_11);
    assert_eq!(inbound.connections(), 4);
}

#[test]
#[cfg_attr(not(feature = "flaky_tests"), ignore)]
fn retry_reconnect_errors() {
    let _ = env_logger::try_init();

    // Used to delay `listen` in the server, to force connection refused errors.
    let (tx, rx) = oneshot::channel();

    let srv = server::http2()
        .route("/", "hello retry")
        .delay_listen(rx.map_err(|_| ()));
    let proxy = proxy::new().inbound(srv).run();
    let client = client::http2(proxy.inbound, "transparency.test.svc.cluster.local");
    let metrics = client::http1(proxy.metrics, "localhost");

    let fut = client.request_async(client.request_builder("/")
        .version(http::Version::HTTP_2));

    // wait until metrics has seen our connection, this can be flaky depending on
    // all the other threads currently running...
    assert_eventually_contains!(
        metrics.get("/metrics"),
        "tcp_open_total{direction=\"inbound\",peer=\"src\",tls=\"disabled\"} 1"
    );

    drop(tx); // start `listen` now
    let res = fut.wait().expect("response");
    assert_eq!(res.status(), http::StatusCode::OK);
}
