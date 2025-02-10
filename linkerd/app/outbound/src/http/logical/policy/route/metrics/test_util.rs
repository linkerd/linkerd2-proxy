use linkerd_app_core::{
    metrics::prom::Counter,
    proxy::http::Body,
    svc::{self, http::BoxBody, Service, ServiceExt},
};

pub use crate::test_util::MockBody;

pub async fn send_assert_incremented(
    counter: &Counter,
    handle: &mut Handle,
    svc: &mut svc::BoxHttp,
    req: http::Request<BoxBody>,
    send: impl FnOnce(SendResponse),
) {
    handle.allow(1);
    let init = counter.get();
    svc.ready().await.expect("ready");
    let mut call = svc.call(req);
    let (_req, tx) = tokio::select! {
        _ = (&mut call) => unreachable!(),
        res = handle.next_request() => res.unwrap(),
    };
    assert_eq!(counter.get(), init);
    send(tx);
    if let Ok(mut rsp) = call.await {
        if !rsp.body().is_end_stream() {
            assert_eq!(counter.get(), 0);
            while let Some(Ok(_)) = rsp.body_mut().data().await {}
            let _ = rsp.body_mut().trailers().await;
        }
    }
    assert_eq!(counter.get(), init + 1);
}

pub type Handle = tower_test::mock::Handle<http::Request<BoxBody>, http::Response<BoxBody>>;
pub type SendResponse = tower_test::mock::SendResponse<http::Response<BoxBody>>;
