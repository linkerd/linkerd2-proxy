//! Unit tests for [`NewRecordStatusCode<N, X, ML, L>`].

use self::util::*;
use super::*;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use http_body_util::BodyExt;
use linkerd_mock_http_body::MockBody;
use linkerd_stack::{layer::Layer, NewService, ServiceExt};
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Code;

/// Demonstrates that multiple status codes can be recorded properly.
#[tokio::test]
async fn multiple_http_statuses() {
    let family: CounterFamily<Labels> = Family::default();

    let mk_rsp: SharedCallback = Arc::new(Mutex::new(Box::new(|| {
        Response::builder()
            .status(StatusCode::OK)
            .body(MockBody::default().then_yield_data(Poll::Ready(Some(Ok("hello".into())))))
            .map(Ok)
            .unwrap()
    })));

    let mk_svc = {
        let mock = NewMockService::new(mk_rsp.clone());
        let extract = Extract(StatusMetrics {
            counters: family.clone(),
        });
        let status = NewTestService::layer_via(extract);
        status.layer(mock).check_new_service()
    };

    {
        // Send one request through, and get a `200` back.
        let svc = mk_svc.new_service(Target);
        let req = Request::builder()
            .body(MockBody::default())
            .expect("a valid request");
        let rsp = svc.oneshot(req).await.expect("request succeeds");
        debug_assert_eq!(rsp.status(), StatusCode::OK);
        rsp.into_body().collect().await.expect("a body");
    }

    // Respond with `307 Temporary Redirect`.
    *mk_rsp.lock().await = Box::new(|| {
        Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .body(MockBody::default().then_yield_data(Poll::Ready(Some(Ok("hello".into())))))
            .map(Ok)
            .unwrap()
    });

    {
        // Send a second request through, and get a `307` back.
        let svc = mk_svc.new_service(Target);
        let req = Request::builder()
            .body(MockBody::default())
            .expect("a valid request");
        let rsp = svc.oneshot(req).await.expect("request succeeds");
        debug_assert_eq!(rsp.status(), StatusCode::TEMPORARY_REDIRECT);
        rsp.into_body().collect().await.expect("a body");
    }

    let ok = family
        .get(&Labels {
            grpc: None,
            http: Some(200),
        })
        .expect("a counter")
        .get();
    let tr = family
        .get(&Labels {
            grpc: None,
            http: Some(307),
        })
        .expect("a counter")
        .get();
    assert_eq!(ok, 1, "one `OK` response handled");
    assert_eq!(tr, 1, "one `Temporary Redirect` response handled");
}

/// Demonstrates that an `200 OK` HTTP status is recorded properly.
#[tokio::test]
async fn http_ok() {
    let family: CounterFamily<Labels> = Family::default();

    let mk_svc = {
        let mk_rsp = || {
            Response::builder()
                .status(StatusCode::OK)
                .body(MockBody::default().then_yield_data(Poll::Ready(Some(Ok("hello".into())))))
                .map(Ok)
                .unwrap()
        };
        let mock = mk_rsp.into();
        let extract = Extract(StatusMetrics {
            counters: family.clone(),
        });
        let status = NewTestService::layer_via(extract);
        status.layer(mock).check_new_service()
    };

    let svc = mk_svc.new_service(Target);

    let req = Request::builder()
        .body(MockBody::default())
        .expect("a valid request");

    let rsp = svc.oneshot(req).await.expect("request succeeds");
    debug_assert_eq!(rsp.status(), StatusCode::OK);
    rsp.into_body().collect().await.expect("a body");

    let n = family
        .get(&Labels {
            grpc: None,
            http: Some(200),
        })
        .expect("a counter")
        .get();
    assert_eq!(n, 1, "one `200 OK` response handled");
}

/// Demonstrates that an error is recorded properly.
#[tokio::test]
async fn http_rsp_error() {
    let family: CounterFamily<Labels> = Family::default();

    let mk_svc = {
        let mk_rsp = || Err("oh no".into());
        let mock = mk_rsp.into();
        let extract = Extract(StatusMetrics {
            counters: family.clone(),
        });
        let status = NewTestService::layer_via(extract);
        status.layer(mock).check_new_service()
    };

    let svc = mk_svc.new_service(Target);

    let req = Request::builder()
        .body(MockBody::default())
        .expect("a valid request");

    svc.oneshot(req)
        .await
        .map(|_| ())
        .expect_err("request fails");

    // a status will not be recorded if an error occurs.
    let n = family
        .get(&Labels {
            grpc: None,
            http: None,
        })
        .expect("a counter")
        .get();
    assert_eq!(n, 1, "one `200 OK` response handled");
}

/// Demonstrates that an error after a status code is observed is recorded properly.
#[tokio::test]
async fn http_body_error() {
    let family: CounterFamily<Labels> = Family::default();

    let mk_svc = {
        let mk_rsp = || {
            Response::builder()
                .status(StatusCode::OK)
                .body(MockBody::default().then_yield_data(Poll::Ready(Some(Err("oh no".into())))))
                .map(Ok)
                .unwrap()
        };
        let mock = mk_rsp.into();
        let extract = Extract(StatusMetrics {
            counters: family.clone(),
        });
        let status = NewTestService::layer_via(extract);
        status.layer(mock).check_new_service()
    };

    let svc = mk_svc.new_service(Target);

    let req = Request::builder()
        .body(MockBody::default())
        .expect("a valid request");

    let rsp = svc.oneshot(req).await.expect("request succeeds");
    debug_assert_eq!(rsp.status(), StatusCode::OK);
    rsp.into_body().collect().await.expect_err("an error");

    // a status will still be recorded if an error occurs while polling the body.
    let n = family
        .get(&Labels {
            grpc: None,
            http: Some(200),
        })
        .expect("a counter")
        .get();
    assert_eq!(n, 1, "one `200 OK` response handled");
}

/// Demonstrates that a gRPC status code is recorded properly.
#[tokio::test]
async fn grpc_ok_trailers() {
    let family: CounterFamily<Labels> = Family::default();

    let mk_svc = {
        let trailers: HeaderMap = [(
            HeaderName::from_static("grpc-status"),
            HeaderValue::from_static("0"),
        )]
        .into_iter()
        .collect();
        let mk_rsp = move || {
            Response::builder()
                .status(StatusCode::OK)
                .body(
                    MockBody::default()
                        .then_yield_data(Poll::Ready(Some(Ok("hello".into()))))
                        .then_yield_trailer(Poll::Ready(Some(Ok(trailers.clone())))),
                )
                .map(Ok)
                .unwrap()
        };
        let mock = mk_rsp.into();
        let extract = Extract(StatusMetrics {
            counters: family.clone(),
        });
        let status = NewTestService::layer_via(extract);
        status.layer(mock).check_new_service()
    };

    use linkerd_stack::NewService;
    let svc = mk_svc.new_service(Target);

    let req = Request::builder()
        .body(MockBody::default())
        .expect("a valid request");

    let rsp = svc.oneshot(req).await.expect("request succeeds");
    let collected = rsp.into_body().collect().await.expect("a body");
    debug_assert!(collected.trailers().is_some());

    let mut registry = prometheus_client::registry::Registry::default();
    registry.register("test", "test help", family.clone());
    let mut out = String::new();
    prometheus_client::encoding::text::encode(&mut out, &registry).unwrap();
    println!("{}", out);

    let n = family
        .get(&Labels {
            grpc: Some(Code::Ok as u16),
            http: Some(200),
        })
        .expect("a counter")
        .get();
    assert_eq!(n, 1, "one gRPC Ok response handled");
}

/// Demonstrates that a gRPC status code is recorded properly when the body is dropped early.
#[tokio::test]
async fn grpc_ok_dropped() {
    let family: CounterFamily<Labels> = Family::default();

    let mk_svc = {
        let trailers: HeaderMap = [(
            HeaderName::from_static("grpc-status"),
            HeaderValue::from_static("0"),
        )]
        .into_iter()
        .collect();
        let mk_rsp = move || {
            Response::builder()
                .status(StatusCode::OK)
                .body(
                    MockBody::default()
                        .then_yield_data(Poll::Ready(Some(Ok("hello".into()))))
                        .then_yield_trailer(Poll::Ready(Some(Ok(trailers.clone())))),
                )
                .map(Ok)
                .unwrap()
        };
        let mock = mk_rsp.into();
        let extract = Extract(StatusMetrics {
            counters: family.clone(),
        });
        let status = NewTestService::layer_via(extract);
        status.layer(mock).check_new_service()
    };

    let svc = mk_svc.new_service(Target);

    let req = Request::builder()
        .body(MockBody::default())
        .expect("a valid request");

    let rsp = svc.oneshot(req).await.expect("request succeeds");
    let mut body = rsp.into_body();
    body.frame()
        .await
        .expect("a result")
        .expect("a frame")
        .into_data()
        .expect("a data frame");
    drop(body);

    assert!(
        family
            .get(&Labels {
                grpc: Some(Code::Ok as u16),
                http: Some(200),
            })
            .is_none(),
        "no Ok series should exist if response body was dropped"
    );
    assert_eq!(
        family
            .get(&Labels {
                grpc: None,
                http: Some(200),
            })
            .expect("a time series for no statuses")
            .get(),
        1,
        "one response with no observed status if response body was dropped"
    );
}

/// A [`NewMockService`] and related facilities for use in unit tests.
mod util {
    use std::sync::Arc;

    use super::*;
    use crate::stream_label::status::{
        LabelGrpcStatus, LabelHttpStatus, MkLabelGrpcStatus, MkLabelHttpStatus,
    };
    use prometheus_client::encoding::{EncodeLabel, EncodeLabelSet};

    /// A target type for use in tests.
    pub struct Target;

    pub type NewTestService =
        NewRecordStatusCode<NewMockService, Extract, TestMkStreamLabel, Labels>;

    pub type Params = crate::status::Params<TestMkStreamLabel, Labels>;

    /// A [`NewService<T>`] that generates [`MockService`]s.
    pub struct NewMockService {
        mk_rsp: SharedCallback,
    }

    /// A [`Service<T>`] that returns a response.
    pub struct MockService {
        rsp: Option<Result<Response<MockBody>, Error>>,
    }

    /// An extractor for use in tests.
    #[derive(Clone)]
    pub struct Extract(pub StatusMetrics<Labels>);

    /// A [`MkStreamLabel`] labeler maker for use in tests.
    ///
    /// Generates [`TestStreamLabel`] labelers.
    #[derive(Clone)]
    pub struct TestMkStreamLabel {
        pub grpc: MkLabelGrpcStatus,
        pub http: MkLabelHttpStatus,
    }

    /// A [`StreamLabel`] labeler for use in tests.
    ///
    /// Records both HTTP and gRPC status.
    #[derive(Clone)]
    pub struct TestStreamLabel {
        pub grpc: LabelGrpcStatus,
        pub http: LabelHttpStatus,
    }

    #[derive(Clone, PartialEq, Eq, std::fmt::Debug, std::hash::Hash)]
    pub struct Labels {
        pub grpc: Option<u16>,
        pub http: Option<u16>,
    }

    pub type SharedCallback = Arc<Mutex<Callback>>;
    pub type Callback = Box<dyn Fn() -> Result<Response<MockBody>, Error> + Send + Sync>;

    // === impl NewMockService ===

    impl NewMockService {
        pub fn new(mk_rsp: SharedCallback) -> Self {
            Self { mk_rsp }
        }
    }

    impl NewService<Target> for NewMockService {
        type Service = MockService;
        fn new_service(&self, _: Target) -> Self::Service {
            let Self { mk_rsp } = self;

            let mk_rsp = futures::executor::block_on(mk_rsp.lock());
            let rsp = Some(mk_rsp());

            Self::Service { rsp }
        }
    }

    impl<F> From<F> for NewMockService
    where
        F: Fn() -> Result<Response<MockBody>, Error> + Send + Sync + 'static,
    {
        fn from(f: F) -> Self {
            let mk_rsp: SharedCallback = Arc::new(Mutex::new(Box::new(f)));
            Self::new(mk_rsp)
        }
    }

    // === impl MockService ===

    impl Service<Request<MockBody>> for MockService {
        type Response = Response<MockBody>;
        type Error = Error;
        type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: Request<MockBody>) -> Self::Future {
            self.rsp
                .take()
                .map(futures::future::ready)
                .expect("service is only called once")
        }
    }

    // === impl ExtractHttp ===

    impl ExtractParam<Params, Target> for Extract {
        fn extract_param(&self, _: &Target) -> Params {
            let Self(metrics) = self;

            let metrics = metrics.clone();
            let mk_stream_label = TestMkStreamLabel {
                grpc: MkLabelGrpcStatus,
                http: MkLabelHttpStatus,
            };

            Params {
                mk_stream_label,
                metrics,
            }
        }
    }

    // === impl TestMkStreamLabel ===

    impl MkStreamLabel for TestMkStreamLabel {
        type StreamLabel = TestStreamLabel;
        type DurationLabels = ();
        type StatusLabels = Labels;

        fn mk_stream_labeler<B>(&self, req: &http::Request<B>) -> Option<Self::StreamLabel> {
            let Self { grpc, http } = self;

            Some(TestStreamLabel {
                grpc: grpc.mk_stream_labeler(req).unwrap(),
                http: http.mk_stream_labeler(req).unwrap(),
            })
        }
    }

    // === impl TestStreamLabel ===

    impl StreamLabel for TestStreamLabel {
        type DurationLabels = ();
        type StatusLabels = Labels;

        fn init_response<B>(&mut self, rsp: &http::Response<B>) {
            let Self { grpc, http } = self;
            grpc.init_response(rsp);
            http.init_response(rsp);
        }

        fn end_response(&mut self, trailers: Result<Option<&http::HeaderMap>, &Error>) {
            let Self { grpc, http } = self;
            grpc.end_response(trailers);
            http.end_response(trailers);
        }

        fn status_labels(&self) -> Self::StatusLabels {
            let Self { grpc, http } = self;
            let grpc = grpc.status_labels().map(|c| c as u16);
            let http = http.status_labels().map(|c| c.as_u16());
            Labels { grpc, http }
        }

        fn duration_labels(&self) -> Self::DurationLabels {}
    }

    // === impl Labels ===

    impl EncodeLabelSet for Labels {
        fn encode(
            &self,
            mut encoder: prometheus_client::encoding::LabelSetEncoder<'_>,
        ) -> Result<(), std::fmt::Error> {
            let Self { grpc, http } = self;

            ("grpc", *grpc).encode(encoder.encode_label())?;
            ("http", *http).encode(encoder.encode_label())?;

            Ok(())
        }
    }
}
