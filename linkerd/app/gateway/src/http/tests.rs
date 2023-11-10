use super::*;
use linkerd_app_core::{
    proxy::http,
    svc::{NewService, ServiceExt},
    tls,
    trace::test::trace_init,
    Error, NameAddr,
};
use linkerd_app_inbound::GatewayLoop;
use linkerd_proxy_server_policy as policy;
use std::{str::FromStr, sync::Arc, time};
use tower_test::mock;

#[tokio::test]
async fn gateway() {
    assert_eq!(
        Test::default().run().await.unwrap().status(),
        http::StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn forward_loop() {
    let test = Test {
        orig_fwd: Some(
            "by=gateway.id.test;for=client.id.test;host=dst.test.example.com:4321;proto=https",
        ),
        ..Default::default()
    };
    let e = test.run().await.unwrap_err();
    assert!(e.is::<GatewayLoop>());
}

/// If the HTTP stack is misconfigured--specifically if two server stacks are
/// instrumented--it may incorrectly determine that an upgraded origin-form
/// HTTP/1.1 request should be in absolute-form.
///
/// This test validates that origin-form requests are not marked as
/// absolute-form.
#[tokio::test(flavor = "current_thread")]
async fn upgraded_request_remains_relative_form() {
    let _trace = trace_init();

    #[derive(Clone, Debug)]
    struct Target;

    impl svc::Param<GatewayAddr> for Target {
        fn param(&self) -> GatewayAddr {
            GatewayAddr("web.test.example.com:80".parse().unwrap())
        }
    }

    impl svc::Param<OrigDstAddr> for Target {
        fn param(&self) -> OrigDstAddr {
            OrigDstAddr(([10, 10, 10, 10], 4143).into())
        }
    }

    impl svc::Param<Remote<ClientAddr>> for Target {
        fn param(&self) -> Remote<ClientAddr> {
            Remote(ClientAddr(([10, 10, 10, 11], 41430).into()))
        }
    }

    impl svc::Param<ServerLabel> for Target {
        fn param(&self) -> ServerLabel {
            ServerLabel(policy::Meta::new_default("test"))
        }
    }

    impl svc::Param<tls::ClientId> for Target {
        fn param(&self) -> tls::ClientId {
            tls::ClientId::from_str("client.id.test").unwrap()
        }
    }

    impl svc::Param<tls::ConditionalServerTls> for Target {
        fn param(&self) -> tls::ConditionalServerTls {
            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(self.param()),
                negotiated_protocol: None,
            })
        }
    }

    impl svc::Param<Option<profiles::Receiver>> for Target {
        fn param(&self) -> Option<profiles::Receiver> {
            Some(linkerd_app_test::profile::only(profiles::Profile {
                addr: Some(profiles::LogicalAddr(
                    "web.test.example.com:80".parse().unwrap(),
                )),
                ..profiles::Profile::default()
            }))
        }
    }

    impl svc::Param<Option<watch::Receiver<profiles::Profile>>> for Target {
        fn param(&self) -> Option<watch::Receiver<profiles::Profile>> {
            svc::Param::<Option<profiles::Receiver>>::param(self).map(Into::into)
        }
    }

    impl svc::Param<http::Version> for Target {
        fn param(&self) -> http::Version {
            http::Version::H2
        }
    }

    impl svc::Param<http::normalize_uri::DefaultAuthority> for Target {
        fn param(&self) -> http::normalize_uri::DefaultAuthority {
            http::normalize_uri::DefaultAuthority(Some(
                http::uri::Authority::from_str("web.test.example.com").unwrap(),
            ))
        }
    }

    impl svc::Param<inbound::policy::AllowPolicy> for Target {
        fn param(&self) -> inbound::policy::AllowPolicy {
            let policy = policy::ServerPolicy {
                meta: inbound::policy::Meta::new_default("test"),
                protocol: policy::Protocol::Detect {
                    timeout: time::Duration::from_secs(10),
                    tcp_authorizations: Arc::new([]),
                    http: Arc::new([policy::http::default(Arc::new([policy::Authorization {
                        authentication: policy::Authentication::TlsUnauthenticated,
                        networks: vec![svc::Param::<Remote<ClientAddr>>::param(self).ip().into()],
                        meta: Arc::new(policy::Meta::Resource {
                            group: "policy.linkerd.io".into(),
                            kind: "authorizationpolicy".into(),
                            name: "testsaz".into(),
                        }),
                    }]))]),
                },
            };
            let (policy, tx) = inbound::policy::AllowPolicy::for_test(self.param(), policy);
            tokio::spawn(async move {
                tx.closed().await;
            });
            policy
        }
    }

    let (inner, mut handle) =
        mock::pair::<http::Request<http::BoxBody>, http::Response<http::BoxBody>>();
    handle.allow(1);

    let outer = {
        let (outbound, _outbound_shutdown) = crate::Outbound::for_test();
        let (inbound, _inbound_shutdown) = crate::Inbound::for_test();
        let gateway = Gateway {
            inbound,
            outbound,
            config: crate::Config {
                allow_discovery: std::iter::once("example.com".parse().unwrap()).collect(),
            },
        };

        let resolve = linkerd_app_test::resolver::Dst::default().endpoint_exists(
            svc::Param::<GatewayAddr>::param(&Target).0,
            svc::Param::<Remote<ClientAddr>>::param(&Target).into(),
            Metadata::default(),
        );
        gateway
            .http(
                svc::ArcNewHttp::new(move |_: _| svc::BoxHttp::new(inner.clone())),
                resolve,
            )
            .new_service(Target)
    };

    // Process a request in the background so we can handle it ourselves to see
    // how the request was mutated
    tokio::spawn(async move {
        let (handle, _closed) =
            http::ClientHandle::new(svc::Param::<Remote<ClientAddr>>::param(&Target).into());
        let req = http::Request::builder()
            .method(http::Method::GET)
            .version(::http::Version::HTTP_2)
            .uri("http://web-original.test.example.com/test.txt")
            .header("l5d-orig-proto", "HTTP/1.1")
            .extension(handle)
            .body(http::BoxBody::default())
            .unwrap();
        let _rsp = outer.oneshot(req).await.unwrap();
        drop(_closed);
    });

    let (request, _respond) = handle.next_request().await.unwrap();
    assert!(request
        .extensions()
        .get::<http::h1::WasAbsoluteForm>()
        .is_none());
}

struct Test {
    target: NameAddr,
    client_id: tls::ClientId,
    orig_fwd: Option<&'static str>,
}

impl Default for Test {
    fn default() -> Self {
        Self {
            target: NameAddr::from_str("dst.test.example.com:4321").unwrap(),
            client_id: tls::ClientId::from_str("client.id.test").unwrap(),
            orig_fwd: None,
        }
    }
}

impl Test {
    async fn run(self) -> Result<http::Response<http::BoxBody>, Error> {
        let Self {
            target,
            client_id,
            orig_fwd,
            ..
        } = self;

        let (outbound, mut handle) =
            mock::pair::<http::Request<http::BoxBody>, http::Response<http::BoxBody>>();

        let new = NewHttpGateway::new(
            move |_: _| outbound.clone(),
            "gateway.id.test".parse().unwrap(),
        );

        #[derive(Clone, Debug)]
        struct Target {
            addr: NameAddr,
            client_id: tls::ClientId,
        }

        impl svc::Param<GatewayAddr> for Target {
            fn param(&self) -> GatewayAddr {
                GatewayAddr(self.addr.clone())
            }
        }

        impl svc::Param<tls::ClientId> for Target {
            fn param(&self) -> tls::ClientId {
                self.client_id.clone()
            }
        }

        let gateway = svc::stack(new)
            .check_new_service::<Target, http::Request<http::BoxBody>>()
            .new_service(Target {
                addr: target.clone(),
                client_id,
            });

        let bg = tokio::spawn(async move {
            handle.allow(1);
            let (req, rsp) = handle.next_request().await.unwrap();
            assert_eq!(
                req.headers().get(http::header::FORWARDED).unwrap(),
                "by=gateway.id.test;for=client.id.test;host=dst.test.example.com:4321;proto=https"
            );
            rsp.send_response(
                http::Response::builder()
                    .status(http::StatusCode::NO_CONTENT)
                    .body(Default::default())
                    .unwrap(),
            );
        });

        let req = http::Request::builder().uri(format!("http://{target}"));
        let req = orig_fwd
            .into_iter()
            .fold(req, |req, fwd| req.header(http::header::FORWARDED, fwd))
            .body(Default::default())
            .unwrap();
        let rsp = gateway.oneshot(req).await?;
        bg.await?;
        Ok(rsp)
    }
}
