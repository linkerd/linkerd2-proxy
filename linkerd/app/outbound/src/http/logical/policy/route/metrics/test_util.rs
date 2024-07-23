use hyper::body::HttpBody;
use linkerd_app_core::{
    metrics::prom::Counter,
    svc::{self, http::BoxBody, Service, ServiceExt},
};

pub use self::mock_body::MockBody;

pub async fn send_assert_incremented(
    counter: &Counter,
    handle: &mut Handle,
    svc: &mut svc::BoxHttp,
    req: http::Request<BoxBody>,
    send: impl FnOnce(SendResponse),
) {
    handle.allow(1);
    assert_eq!(counter.get(), 0);
    svc.ready().await.expect("ready");
    let mut call = svc.call(req);
    let (_req, tx) = tokio::select! {
        _ = (&mut call) => unreachable!(),
        res = handle.next_request() => res.unwrap(),
    };
    assert_eq!(counter.get(), 0);
    send(tx);
    if let Ok(mut rsp) = call.await {
        if !rsp.body().is_end_stream() {
            assert_eq!(counter.get(), 0);
            while let Some(Ok(_)) = rsp.body_mut().data().await {}
            let _ = rsp.body_mut().trailers().await;
        }
    }
    assert_eq!(counter.get(), 1);
}

pub type Handle = tower_test::mock::Handle<http::Request<BoxBody>, http::Response<BoxBody>>;
pub type SendResponse = tower_test::mock::SendResponse<http::Response<BoxBody>>;

mod mock_body {
    use bytes::Bytes;
    use hyper::body::HttpBody;
    use linkerd_app_core::{Error, Result};
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    #[derive(Default)]
    #[pin_project::pin_project]
    pub struct MockBody {
        #[pin]
        data: Option<futures::future::BoxFuture<'static, Result<()>>>,
        #[pin]
        trailers: Option<futures::future::BoxFuture<'static, Result<Option<http::HeaderMap>>>>,
    }

    impl MockBody {
        pub fn new(data: impl Future<Output = Result<()>> + Send + 'static) -> Self {
            Self {
                data: Some(Box::pin(data)),
                trailers: None,
            }
        }

        pub fn trailers(
            trailers: impl Future<Output = Result<Option<http::HeaderMap>>> + Send + 'static,
        ) -> Self {
            Self {
                data: None,
                trailers: Some(Box::pin(trailers)),
            }
        }
    }

    impl HttpBody for MockBody {
        type Data = Bytes;
        type Error = Error;

        fn poll_data(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
            let mut this = self.project();
            if let Some(rx) = this.data.as_mut().as_pin_mut() {
                let ready = futures::ready!(rx.poll(cx));
                *this.data = None;
                return Poll::Ready(ready.err().map(Err));
            }
            Poll::Ready(None)
        }

        fn poll_trailers(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<Option<hyper::HeaderMap>, Self::Error>> {
            let mut this = self.project();
            if let Some(rx) = this.trailers.as_mut().as_pin_mut() {
                let ready = futures::ready!(rx.poll(cx));
                *this.trailers = None;
                return Poll::Ready(ready);
            }
            Poll::Ready(Ok(None))
        }

        fn is_end_stream(&self) -> bool {
            self.data.is_none() && self.trailers.is_none()
        }
    }
}
