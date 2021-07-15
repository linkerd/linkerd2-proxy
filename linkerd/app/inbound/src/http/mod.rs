mod set_identity_header;
#[cfg(test)]
mod tests;

use self::set_identity_header::NewSetIdentityHeader;
use crate::{
    allow_discovery::AllowProfile,
    target::{self, HttpAccept, HttpEndpoint, Logical, RequestTarget, Target, TcpEndpoint},
    Inbound,
};
pub use linkerd_app_core::proxy::http::{
    normalize_uri, strip_header, uri, BoxBody, BoxResponse, DetectHttp, Request, Response, Retain,
    Version,
};
use linkerd_app_core::{
    classify,
    config::{ProxyConfig, ServerConfig},
    dst, errors, http_tracing, identity, io, profiles,
    proxy::{http, tap},
    svc::{self, Param},
    Error,
};
use tracing::debug_span;

impl<H> Inbound<H> {
    pub fn push_http_server<T, I, HSvc>(
        self,
    ) -> Inbound<
        svc::BoxNewService<
            T,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        T: Param<Version>
            + Param<http::normalize_uri::DefaultAuthority>
            + Param<Option<identity::Name>>,
        T: Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
        H: svc::NewService<T, Service = HSvc> + Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Clone
            + Send
            + Unpin
            + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
    {
        let Self {
            config,
            runtime: rt,
            stack: http,
        } = self;
        let ProxyConfig {
            server: ServerConfig { h2_settings, .. },
            dispatch_timeout,
            max_in_flight_requests,
            ..
        } = config.proxy;

        let stack = http
            .check_new_service::<T, http::Request<_>>()
            // Convert origin form HTTP/1 URIs to absolute form for Hyper's
            // `Client`. This must be below the `orig_proto::Downgrade` layer, since
            // the request may have been downgraded from a HTTP/2 orig-proto request.
            .push(http::NewNormalizeUri::layer())
            .push(NewSetIdentityHeader::layer())
            .push_on_response(
                svc::layers()
                    // Downgrades the protocol if upgraded by an outbound proxy.
                    .push(http::orig_proto::Downgrade::layer())
                    // Limit the number of in-flight requests. When the proxy is
                    // at capacity, go into failfast after a dispatch timeout.
                    // Note that the inner service _always_ returns ready (due
                    // to `NewRouter`) and the concurrency limit need not be
                    // driven outside of the request path, so there's no need
                    // for SpawnReady
                    .push(svc::ConcurrencyLimitLayer::new(max_in_flight_requests))
                    .push(svc::FailFast::layer("HTTP Server", dispatch_timeout))
                    .push(rt.metrics.http_errors.clone())
                    // Synthesizes responses for proxy errors.
                    .push(errors::layer())
                    .push(http_tracing::server(rt.span_sink.clone(), trace_labels()))
                    // Record when an HTTP/1 URI was in absolute form
                    .push(http::normalize_uri::MarkAbsoluteForm::layer())
                    .push(http::BoxRequest::layer())
                    .push(http::BoxResponse::layer()),
            )
            .check_new_service::<T, http::Request<_>>()
            .instrument(|t: &T| debug_span!("http", v=%Param::<Version>::param(t)))
            .push(http::NewServeHttp::layer(h2_settings, rt.drain.clone()))
            .push(svc::BoxNewService::layer());

        Inbound {
            config,
            runtime: rt,
            stack,
        }
    }
}

impl<C> Inbound<C>
where
    C: svc::Service<TcpEndpoint> + Clone + Send + Sync + Unpin + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
    C::Error: Into<Error>,
    C::Future: Send,
{
    pub fn push_http_router<P>(
        self,
        profiles: P,
    ) -> Inbound<
        svc::BoxNewService<
            HttpAccept,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    >
    where
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + 'static,
        P::Future: Send,
        P::Error: Send,
    {
        let Self {
            config,
            runtime: rt,
            stack: connect,
        } = self;

        // Creates HTTP clients for each inbound port & HTTP settings.
        let endpoint = connect
            .push(svc::stack::BoxFuture::layer()) // Makes the response future `Unpin`.
            .push(rt.metrics.transport.layer_connect())
            .push_map_target(TcpEndpoint::from)
            .push(http::client::layer(
                config.proxy.connect.h1_settings,
                config.proxy.connect.h2_settings,
            ))
            .push_on_response(svc::MapErrLayer::new(Into::into))
            .into_new_service()
            .push_new_reconnect(config.proxy.connect.backoff)
            .check_new_service::<HttpEndpoint, http::Request<_>>();

        let target = endpoint
            .push_map_target(HttpEndpoint::from)
            // Registers the stack to be tapped.
            .push(tap::NewTapHttp::layer(rt.tap.clone()))
            // Records metrics for each `Target`.
            .push(
                rt.metrics
                    .http_endpoint
                    .to_layer::<classify::Response, _, _>(),
            )
            .push_on_response(http_tracing::client(rt.span_sink.clone(), trace_labels()))
            .push_on_response(http::BoxResponse::layer())
            .check_new_service::<Target, http::Request<_>>();

        let no_profile = target
            .clone()
            .push_on_response(http::BoxResponse::layer())
            .check_new_service::<Target, http::Request<_>>()
            .into_inner();
        // Attempts to discover a service profile for each logical target (as
        // informed by the request's headers). The stack is cached until a
        // request has not been received for `cache_max_idle_age`.
        let stack = target
            .clone()
            .check_new_service::<Target, http::Request<http::BoxBody>>()
            .push_on_response(http::BoxRequest::layer())
            // The target stack doesn't use the profile resolution, so drop it.
            .push_map_target(Target::from)
            .push(profiles::http::route_request::layer(
                svc::proxies()
                    // Sets the route as a request extension so that it can be used
                    // by tap.
                    .push_http_insert_target::<dst::Route>()
                    // Records per-route metrics.
                    .push(
                        rt.metrics
                            .http_route
                            .to_layer::<classify::Response, _, dst::Route>(),
                    )
                    // Sets the per-route response classifier as a request
                    // extension.
                    .push(classify::NewClassify::layer())
                    .check_new_clone::<dst::Route>()
                    .push_map_target(target::route)
                    .into_inner(),
            ))
            .push_map_target(Logical::from)
            .push_on_response(http::BoxResponse::layer())
            .check_new_service::<(profiles::Receiver, Target), _>()
            .push(svc::UnwrapOr::layer(no_profile))
            .push(profiles::discover::layer(
                profiles,
                AllowProfile(config.allow_discovery.clone()),
            ))
            .instrument(|_: &Target| debug_span!("profile"))
            .push_on_response(
                svc::layers()
                    .push(http::BoxResponse::layer())
                    .push(svc::layer::mk(svc::SpawnReady::new)),
            )
            // Skip the profile stack if it takes too long to become ready.
            .push_when_unready(
                config.profile_idle_timeout,
                target
                    .clone()
                    .push_on_response(svc::layer::mk(svc::SpawnReady::new))
                    .into_inner(),
            )
            .check_new_service::<Target, http::Request<BoxBody>>()
            .push_on_response(
                svc::layers()
                    .push(
                        rt.metrics
                            .stack
                            .layer(crate::stack_labels("http", "logical")),
                    )
                    .push(svc::FailFast::layer(
                        "HTTP Logical",
                        config.proxy.dispatch_timeout,
                    ))
                    .push_spawn_buffer(config.proxy.buffer_capacity),
            )
            .push_cache(config.proxy.cache_max_idle_age)
            .push_on_response(
                svc::layers()
                    .push(http::Retain::layer())
                    .push(http::BoxResponse::layer()),
            )
            .check_new_service::<Target, http::Request<http::BoxBody>>()
            // Routes each request to a target, obtains a service for that
            // target, and dispatches the request.
            .instrument_from_target()
            .push(svc::BoxNewService::layer())
            .push(svc::NewRouter::layer(RequestTarget::from))
            // Used by tap.
            .push_http_insert_target::<HttpAccept>()
            .push(svc::BoxNewService::layer());

        Inbound {
            config,
            runtime: rt,
            stack,
        }
    }
}

fn trace_labels() -> std::collections::HashMap<String, String> {
    let mut l = std::collections::HashMap::new();
    l.insert("direction".to_string(), "inbound".to_string());
    l
}

#[cfg(fuzzing)]
pub mod fuzz_logic {
    use crate::{
        target::{HttpAccept, TcpAccept, TcpEndpoint},
        test_util::{
            support::{connect::Connect, http_util, profile, resolver},
            *,
        },
        Config, Inbound,
    };
    use hyper::http;
    use hyper::{client::conn::Builder as ClientBuilder, Body, Request, Response};
    use libfuzzer_sys::arbitrary::Arbitrary;
    use linkerd_app_core::{
        io, proxy,
        svc::{self, NewService, Param},
        tls,
        transport::{ClientAddr, Remote, ServerAddr},
        Conditional, NameAddr, ProxyRuntime,
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
        let accept = HttpAccept {
            version: proxy::http::Version::Http1,
            tcp: TcpAccept {
                target_addr: ([127, 0, 0, 1], 5550).into(),
                client_addr: Remote(ClientAddr(([10, 0, 0, 41], 6894).into())),
                tls: Conditional::None(tls::server::NoServerTls::NoClientHello),
            },
        };
        let connect =
            support::connect().endpoint_fn_boxed(accept.tcp.target_addr, hello_fuzz_server(server));
        let profiles = profile::resolver();
        let profile_tx = profiles
            .profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
        profile_tx.send(profile::Profile::default()).unwrap();

        // Build the outbound server
        let cfg = default_config();
        let (rt, _shutdown) = runtime();
        let server = build_fuzz_server(cfg, rt, profiles, connect).new_service(accept);
        let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

        // Now send all of the requests
        for inp in requests.iter() {
            if let Ok(uri) = std::str::from_utf8(&inp.uri[..]) {
                if let Ok(header_name) = std::str::from_utf8(&inp.header_name[..]) {
                    if let Ok(header_value) = std::str::from_utf8(&inp.header_value[..]) {
                        let http_method = if inp.http_method {
                            http::Method::GET
                        } else {
                            http::Method::POST
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
        // indcate that the proxy closed the connection --- which it will do on
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
    ) -> impl svc::NewService<
        HttpAccept,
        Service = impl tower::Service<
            I,
            Response = (),
            Error = impl Into<linkerd_app_core::Error>,
            Future = impl Send + 'static,
        > + Send
                      + Clone,
    > + Clone
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
    {
        let connect = svc::stack(connect)
            .push_map_target(|t: TcpEndpoint| {
                Remote(ServerAddr(([127, 0, 0, 1], t.param()).into()))
            })
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
}
