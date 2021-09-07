mod router;
mod server;
mod set_identity_header;
#[cfg(test)]
mod tests;

use crate::{
    policy::DeniedUnauthorized, GatewayDomainInvalid, GatewayIdentityRequired, GatewayLoop,
};
use linkerd_app_core::{errors, Error, Result};

#[derive(Copy, Clone)]
pub(crate) struct HttpRescue;

// === impl HttpRescue ===

impl HttpRescue {
    /// Synthesizes responses for HTTP requests that encounter proxy errors.
    pub fn layer() -> errors::respond::Layer<Self> {
        errors::respond::NewRespond::layer(Self)
    }
}

impl errors::HttpRescue<Error> for HttpRescue {
    fn rescue(&self, error: Error) -> Result<errors::SyntheticHttpResponse> {
        if Self::has_cause::<DeniedUnauthorized>(&*error) {
            return Ok(errors::SyntheticHttpResponse {
                http_status: http::StatusCode::FORBIDDEN,
                grpc_status: tonic::Code::PermissionDenied,
                close_connection: true,
                message: error.to_string(),
            });
        }

        if Self::has_cause::<GatewayDomainInvalid>(&*error) {
            return Ok(errors::SyntheticHttpResponse {
                http_status: http::StatusCode::BAD_REQUEST,
                grpc_status: tonic::Code::InvalidArgument,
                close_connection: true,
                message: error.to_string(),
            });
        }

        if Self::has_cause::<GatewayIdentityRequired>(&*error) {
            return Ok(errors::SyntheticHttpResponse {
                http_status: http::StatusCode::FORBIDDEN,
                grpc_status: tonic::Code::Unauthenticated,
                close_connection: true,
                message: error.to_string(),
            });
        }

        if Self::has_cause::<GatewayLoop>(&*error) {
            return Ok(errors::SyntheticHttpResponse {
                http_status: http::StatusCode::LOOP_DETECTED,
                grpc_status: tonic::Code::Aborted,
                close_connection: true,
                message: error.to_string(),
            });
        }

        errors::DefaultHttpRescue.rescue(error)
    }
}

// === impl trace_labels ===

fn trace_labels() -> std::collections::HashMap<String, String> {
    let mut l = std::collections::HashMap::new();
    l.insert("direction".to_string(), "inbound".to_string());
    l
}

#[cfg(fuzzing)]
pub mod fuzz {
    use crate::{
        http::router::Http,
        policy,
        test_util::{
            support::{connect::Connect, http_util, profile, resolver},
            *,
        },
        Config, Inbound,
    };
    use hyper::{client::conn::Builder as ClientBuilder, Body, Request, Response};
    use libfuzzer_sys::arbitrary::Arbitrary;
    use linkerd_app_core::{
        identity, io,
        proxy::http,
        svc::{self, NewService, Param},
        tls,
        transport::{ClientAddr, OrigDstAddr, Remote, ServerAddr},
        NameAddr, ProxyRuntime,
    };
    pub use linkerd_app_test as support;
    use linkerd_app_test::*;
    use std::{fmt, str};

    #[derive(Arbitrary)]
    pub struct HttpRequestSpec {
        pub uri: Vec<u8>,
        pub header_name: Vec<u8>,
        pub header_value: Vec<u8>,
        pub http_method: bool,
    }

    pub async fn fuzz_entry_raw(requests: Vec<HttpRequestSpec>) {
        let mut server = hyper::server::conn::Http::new();
        server.http1_only(true);
        let mut client = ClientBuilder::new();
        let connect =
            support::connect().endpoint_fn_boxed(Target::addr(), hello_fuzz_server(server));
        let profiles = profile::resolver();
        let profile_tx = profiles
            .profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
        profile_tx.send(profile::Profile::default()).unwrap();

        // Build the outbound server
        let cfg = default_config();
        let (rt, _shutdown) = runtime();
        let server = build_fuzz_server(cfg, rt, profiles, connect).new_service(Target::HTTP1);
        let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

        // Now send all of the requests
        for inp in requests.iter() {
            if let Ok(uri) = std::str::from_utf8(&inp.uri[..]) {
                if let Ok(header_name) = std::str::from_utf8(&inp.header_name[..]) {
                    if let Ok(header_value) = std::str::from_utf8(&inp.header_value[..]) {
                        let http_method = if inp.http_method {
                            hyper::http::Method::GET
                        } else {
                            hyper::http::Method::POST
                        };

                        if let Ok(req) = Request::builder()
                            .method(http_method)
                            .uri(uri)
                            .header(header_name, header_value)
                            .body(Body::default())
                        {
                            let rsp = http_util::http_request(&mut client, req).await;
                            tracing::info!(?rsp);
                            if let Ok(rsp) = rsp {
                                let body = http_util::body_to_string(rsp.into_body()).await;
                                tracing::info!(?body);
                            }
                        }
                    }
                }
            }
        }

        drop(client);
        // It's okay if the background task returns an error, as this would
        // indicate that the proxy closed the connection --- which it will do on
        // invalid inputs. We want to ensure that the proxy doesn't crash in the
        // face of these inputs, and the background task will panic in this
        // case.
        let res = bg.await;
        tracing::info!(?res, "background tasks completed")
    }

    fn hello_fuzz_server(
        http: hyper::server::conn::Http,
    ) -> impl Fn(Remote<ServerAddr>) -> io::Result<io::BoxedIo> {
        move |_endpoint| {
            let (client_io, server_io) = support::io::duplex(4096);
            let hello_svc = hyper::service::service_fn(|_request: Request<Body>| async move {
                Ok::<_, io::Error>(Response::new(Body::from("Hello world!")))
            });
            tokio::spawn(
                http.serve_connection(server_io, hello_svc)
                    .in_current_span(),
            );
            Ok(io::BoxedIo::new(client_io))
        }
    }

    fn build_fuzz_server<I>(
        cfg: Config,
        rt: ProxyRuntime,
        profiles: resolver::Profiles,
        connect: Connect<Remote<ServerAddr>>,
    ) -> svc::BoxNewTcp<Target, I>
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
    {
        let connect = svc::stack(connect)
            .push_map_target(|t: Http| Remote(ServerAddr(([127, 0, 0, 1], t.param()).into())))
            .into_inner();
        Inbound::new(cfg, rt)
            .with_stack(connect)
            .push_http_router(profiles)
            .push_http_server()
            .into_inner()
    }

    impl fmt::Debug for HttpRequestSpec {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // Custom `Debug` impl that formats the URI, header name, and header
            // value as strings if they are UTF-8, or falls back to raw bytes
            // otherwise.
            let mut dbg = f.debug_struct("HttpRequestSpec");
            dbg.field("http_method", &self.http_method);

            if let Ok(uri) = str::from_utf8(&self.uri[..]) {
                dbg.field("uri", &uri);
            } else {
                dbg.field("uri", &self.uri);
            }

            if let Ok(name) = str::from_utf8(&self.header_name[..]) {
                dbg.field("header_name", &name);
            } else {
                dbg.field("header_name", &self.header_name);
            }

            if let Ok(value) = str::from_utf8(&self.header_value[..]) {
                dbg.field("header_value", &value);
            } else {
                dbg.field("header_value", &self.header_value);
            }

            dbg.finish()
        }
    }

    #[derive(Clone, Debug)]
    struct Target(http::Version);

    // === impl Target ===

    impl Target {
        const HTTP1: Self = Self(http::Version::Http1);

        fn addr() -> SocketAddr {
            ([127, 0, 0, 1], 80).into()
        }
    }

    impl svc::Param<OrigDstAddr> for Target {
        fn param(&self) -> OrigDstAddr {
            OrigDstAddr(Self::addr())
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

    impl svc::Param<http::Version> for Target {
        fn param(&self) -> http::Version {
            self.0
        }
    }

    impl svc::Param<tls::ConditionalServerTls> for Target {
        fn param(&self) -> tls::ConditionalServerTls {
            tls::ConditionalServerTls::None(tls::NoServerTls::NoClientHello)
        }
    }

    impl svc::Param<policy::AllowPolicy> for Target {
        fn param(&self) -> policy::AllowPolicy {
            let (policy, _) = policy::AllowPolicy::for_test(
                self.param(),
                policy::ServerPolicy {
                    protocol: policy::Protocol::Http1,
                    authorizations: vec![policy::Authorization {
                        authentication: policy::Authentication::Unauthenticated,
                        networks: vec![std::net::IpAddr::from([192, 0, 2, 3]).into()],
                        name: "testsaz".to_string(),
                    }],
                    name: "testsrv".to_string(),
                },
            );
            policy
        }
    }

    impl svc::Param<policy::ServerLabel> for Target {
        fn param(&self) -> policy::ServerLabel {
            policy::ServerLabel("testsrv".to_string())
        }
    }

    impl svc::Param<http::normalize_uri::DefaultAuthority> for Target {
        fn param(&self) -> http::normalize_uri::DefaultAuthority {
            http::normalize_uri::DefaultAuthority(None)
        }
    }

    impl svc::Param<Option<identity::Name>> for Target {
        fn param(&self) -> Option<identity::Name> {
            None
        }
    }
}
