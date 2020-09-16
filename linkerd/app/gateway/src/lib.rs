#![deny(warnings, rust_2018_idioms)]

mod config;
mod gateway;
mod make;

pub use self::config::Config;

#[cfg(test)]
mod test {
    use super::*;
    use linkerd2_app_core::proxy::{http, identity};
    use linkerd2_app_core::{dns, errors::HttpError, transport::tls, Error, NameAddr, Never};
    use linkerd2_app_inbound::endpoint as inbound;
    //use linkerd2_app_outbound::endpoint as outbound;
    use std::{
        convert::TryFrom,
        net::{IpAddr, SocketAddr},
    };
    use tokio_test::assert_ready_ok;
    use tower::util::{service_fn, ServiceExt};
    use tower_test::mock;

    #[tokio::test]
    async fn gateway() {
        assert!(Test::default().run().await.is_ok());
    }

    #[tokio::test]
    async fn bad_domain() {
        let test = Test {
            suffix: "bad.example.com",
            ..Default::default()
        };
        let status = test
            .run()
            .await
            .unwrap_err()
            .downcast_ref::<HttpError>()
            .unwrap()
            .status();
        assert_eq!(status, http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn no_authority() {
        let test = Test {
            dst_name: None,
            ..Default::default()
        };
        let status = test
            .run()
            .await
            .unwrap_err()
            .downcast_ref::<HttpError>()
            .unwrap()
            .status();
        assert_eq!(status, http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn no_identity() {
        let peer_id = tls::PeerIdentity::None(tls::ReasonForNoPeerName::NoPeerIdFromRemote);
        let test = Test {
            peer_id,
            ..Default::default()
        };
        let status = test
            .run()
            .await
            .unwrap_err()
            .downcast_ref::<HttpError>()
            .unwrap()
            .status();
        assert_eq!(status, http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn forward_loop() {
        let test = Test {
            orig_fwd: Some(
                "by=gateway.id.test;for=client.id.test;host=dst.test.example.com:4321;proto=https",
            ),
            ..Default::default()
        };
        let status = test
            .run()
            .await
            .unwrap_err()
            .downcast_ref::<HttpError>()
            .unwrap()
            .status();
        assert_eq!(status, http::StatusCode::LOOP_DETECTED);
    }

    struct Test {
        suffix: &'static str,
        dst_name: Option<&'static str>,
        peer_id: tls::PeerIdentity,
        orig_fwd: Option<&'static str>,
    }

    impl Default for Test {
        fn default() -> Self {
            Self {
                suffix: "test.example.com",
                dst_name: Some("dst:4321"),
                peer_id: tls::PeerIdentity::Some(identity::Name::from(
                    dns::Name::try_from("client.id.test".as_bytes()).unwrap(),
                )),
                orig_fwd: None,
            }
        }
    }

    impl Test {
        async fn run(self) -> Result<http::Response<http::boxed::Payload>, Error> {
            let Self {
                suffix,
                dst_name,
                peer_id,
                orig_fwd,
            } = self;

            let (outbound, mut handle) = mock::pair::<
                http::Request<http::boxed::Payload>,
                http::Response<http::boxed::Payload>,
            >();
            let mut make_gateway = {
                let resolve = service_fn(move |short: dns::Name| async move {
                    let long = format!("{}.{}", short.without_trailing_dot(), suffix);
                    Ok::<_, Never>((
                        dns::Name::try_from(long.as_bytes()).unwrap(),
                        IpAddr::from([127, 0, 0, 1]),
                    ))
                });
                mock::Spawn::new(make::MakeGateway::new(
                    resolve,
                    service_fn(move |_: _| {
                        let out = outbound.clone();
                        async move { Ok::<_, Never>(out) }
                    }),
                    tls::PeerIdentity::Some(identity::Name::from(
                        dns::Name::try_from("gateway.id.test".as_bytes()).unwrap(),
                    )),
                    Some(dns::Suffix::try_from("test.example.com.").unwrap()),
                ))
            };

            let socket_addr = SocketAddr::from(([127, 0, 0, 1], 4143));
            let target = inbound::Target {
                socket_addr,
                dst: dst_name
                    .map(|n| NameAddr::from_str(n).unwrap().into())
                    .unwrap_or_else(|| socket_addr.into()),
                http_version: http::Version::Http1,
                tls_client_id: peer_id,
            };
            assert_ready_ok!(make_gateway.poll_ready());
            let gateway = make_gateway.call(target).await.unwrap();

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

            let req = http::Request::builder()
                .uri(format!("http://{}", dst_name.unwrap_or("127.0.0.1:4321")));
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
}
