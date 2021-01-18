#![deny(warnings, rust_2018_idioms)]

mod config;
mod gateway;
mod make;

pub use self::config::Config;

#[cfg(test)]
mod test {
    use super::*;
    use linkerd_app_core::{
        dns, errors::HttpError, identity as id, profiles, proxy::http, svc::NewService, tls, Error,
        NameAddr, NameMatch, Never,
    };
    use linkerd_app_inbound::target as inbound;
    use linkerd_app_test as support;
    use std::{net::SocketAddr, str::FromStr};
    use tower::util::{service_fn, ServiceExt};
    use tower_test::mock;

    #[tokio::test]
    async fn gateway() {
        assert_eq!(
            Test::default().run().await.unwrap().status(),
            http::StatusCode::NO_CONTENT
        );
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
        let client_id = tls::Conditional::None(tls::ReasonForNoPeerName::NoPeerIdFromRemote);
        let test = Test {
            client_id,
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
        client_id: tls::Conditional<tls::ClientId>,
        orig_fwd: Option<&'static str>,
    }

    impl Default for Test {
        fn default() -> Self {
            Self {
                suffix: "test.example.com",
                dst_name: Some("dst.test.example.com:4321"),
                client_id: tls::Conditional::Some(
                    id::Name::from(dns::Name::from_str("client.id.test").unwrap()).into(),
                ),
                orig_fwd: None,
            }
        }
    }

    impl Test {
        async fn run(self) -> Result<http::Response<http::BoxBody>, Error> {
            let Self {
                suffix,
                dst_name,
                client_id,
                orig_fwd,
            } = self;

            let (outbound, mut handle) =
                mock::pair::<http::Request<http::BoxBody>, http::Response<http::BoxBody>>();
            let mut make_gateway = {
                let profiles = service_fn(move |na: NameAddr| async move {
                    let rx = support::profile::only(profiles::Profile {
                        name: Some(na.name().clone()),
                        ..profiles::Profile::default()
                    });
                    Ok::<_, Never>(Some(rx))
                });
                let allow_discovery = NameMatch::new(Some(dns::Suffix::from_str(suffix).unwrap()));
                Config { allow_discovery }.build(
                    move |_: _| outbound.clone(),
                    profiles,
                    Some(tls::LocalId(id::Name::from_str("gateway.id.test").unwrap())),
                )
            };

            let target_addr = SocketAddr::from(([127, 0, 0, 1], 4143));
            let target = inbound::Target {
                target_addr,
                client_id,
                dst: dst_name
                    .map(|n| NameAddr::from_str(n).unwrap().into())
                    .unwrap_or_else(|| target_addr.into()),
                http_version: http::Version::Http1,
            };
            let gateway = make_gateway.new_service(target);

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
            let rsp = gateway.oneshot(req).await.map_err(Into::into)?;
            bg.await?;
            Ok(rsp)
        }
    }
}
