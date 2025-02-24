use http::Response;
use linkerd_app_core::{
    metrics::prom::Counter,
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
    if let Ok(mut body) = call
        .await
        .map(Response::into_body)
        .map(linkerd_http_body_compat::ForwardCompatibleBody::new)
    {
        if !body.is_end_stream() {
            assert_eq!(counter.get(), 0);
            while let Some(Ok(_)) = body.frame().await {}
        }
    }
    assert_eq!(counter.get(), init + 1);
}

pub type Handle = tower_test::mock::Handle<http::Request<BoxBody>, http::Response<BoxBody>>;
pub type SendResponse = tower_test::mock::SendResponse<http::Response<BoxBody>>;
