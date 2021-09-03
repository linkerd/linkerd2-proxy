use super::*;
use linkerd_app_core::{
    dns, errors::HttpError, identity as id, profiles, proxy::http, svc::NewService, tls, Error,
    NameAddr, NameMatch,
};
use linkerd_app_inbound::{GatewayDomainInvalid, GatewayIdentityRequired, GatewayLoop};
use linkerd_app_test as support;
use std::{error::Error as _, str::FromStr};
use tower::util::ServiceExt;
use tower_test::mock;

#[tokio::test]
async fn gateway() {
    assert_eq!(
        Test::default()
            .with_default_profile()
            .run()
            .await
            .unwrap()
            .status(),
        http::StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn gateway_endpoint() {
    let addr = std::net::SocketAddr::new([192, 0, 2, 10].into(), 777);
    let profile = support::profile::only(profiles::Profile {
        endpoint: Some((addr, Metadata::default())),
        ..profiles::Profile::default()
    });

    assert_eq!(
        Test::default()
            .with_profile(profile)
            .run()
            .await
            .unwrap()
            .status(),
        http::StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn bad_domain() {
    let test = Test {
        suffix: "bad.example.com",
        ..Default::default()
    };
    let e = test.with_default_profile().run().await.unwrap_err();
    assert!(e
        .downcast::<HttpError>()
        .unwrap()
        .source()
        .unwrap()
        .is::<GatewayDomainInvalid>());
}

#[tokio::test]
async fn no_identity() {
    let test = Test {
        client_id: None,
        ..Default::default()
    };
    let e = test.with_default_profile().run().await.unwrap_err();
    assert!(e
        .downcast::<HttpError>()
        .unwrap()
        .source()
        .unwrap()
        .is::<GatewayIdentityRequired>());
}

#[tokio::test]
async fn forward_loop() {
    let test = Test {
        orig_fwd: Some(
            "by=gateway.id.test;for=client.id.test;host=dst.test.example.com:4321;proto=https",
        ),
        ..Default::default()
    };
    let e = test.with_default_profile().run().await.unwrap_err();
    assert!(e
        .downcast::<HttpError>()
        .unwrap()
        .source()
        .unwrap()
        .is::<GatewayLoop>());
}

struct Test {
    suffix: &'static str,
    target: NameAddr,
    client_id: Option<tls::ClientId>,
    orig_fwd: Option<&'static str>,
    profile: Option<profiles::Receiver>,
}

impl Default for Test {
    fn default() -> Self {
        Self {
            suffix: "test.example.com",
            target: NameAddr::from_str("dst.test.example.com:4321").unwrap(),
            client_id: Some(tls::ClientId::from_str("client.id.test").unwrap()),
            orig_fwd: None,
            profile: None,
        }
    }
}

impl Test {
    async fn run(self) -> Result<http::Response<http::BoxBody>, Error> {
        let Self {
            suffix: _,
            target,
            client_id,
            orig_fwd,
            profile,
        } = self;

        let (outbound, mut handle) =
            mock::pair::<http::Request<http::BoxBody>, http::Response<http::BoxBody>>();

        let new = NewGateway::new(
            move |_: svc::Either<outbound::http::Logical, outbound::http::Endpoint>| {
                outbound.clone()
            },
            Some(tls::LocalId(id::Name::from_str("gateway.id.test").unwrap())),
        );

        let t = HttpTarget {
            target: target.clone(),
            version: http::Version::Http1,
        };
        let gateway = svc::stack(new)
            .check_new_service::<gateway::Target, http::Request<http::BoxBody>>()
            .new_service((profile, t));

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

        let req = http::Request::builder().uri(format!("http://{}", target));
        let mut req = orig_fwd
            .into_iter()
            .fold(req, |req, fwd| req.header(http::header::FORWARDED, fwd))
            .body(Default::default())
            .unwrap();
        if let Some(id) = client_id {
            req.extensions_mut().insert(id);
        }
        let rsp = gateway.oneshot(req).await?;
        bg.await?;
        Ok(rsp)
    }

    fn with_profile(mut self, profile: profiles::Receiver) -> Self {
        let allow = Some(dns::Suffix::from_str(self.suffix).unwrap())
            .into_iter()
            .collect::<NameMatch>();
        if allow.matches(self.target.name()) {
            self.profile = Some(profile);
        }
        self
    }

    fn with_default_profile(self) -> Self {
        let target = self.target.clone();
        self.with_profile(support::profile::only(profiles::Profile {
            addr: Some(target.into()),
            ..profiles::Profile::default()
        }))
    }
}
