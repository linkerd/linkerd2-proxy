use super::*;
use std::time::Duration;
use tokio_test::{assert_pending, assert_ready, assert_ready_err, assert_ready_ok, task};
use tower_test::mock::{self, Spawn};

#[tokio::test]
async fn fails_fast() {
    let _trace = linkerd_tracing::test::trace_init();
    tokio::time::pause();

    let max_unavailable = Duration::from_millis(100);
    let (service, mut handle) = mock::pair::<(), ()>();
    let shared = Arc::new(Shared {
        notify: tokio::sync::Notify::new(),
        in_failfast: AtomicBool::new(false),
    });
    let mut service = Spawn::new(FailFast::new(max_unavailable, shared, service));

    // The inner starts unavailable.
    handle.allow(0);
    assert_pending!(service.poll_ready());

    // Then we wait for the idle timeout, at which point the service
    // should start failing fast.
    tokio::time::sleep(max_unavailable + Duration::from_millis(1)).await;
    assert_ready_ok!(service.poll_ready());

    let err = service.call(()).await.expect_err("should failfast");
    assert!(err.is::<super::FailFastError>());

    // Then the inner service becomes available.
    handle.allow(1);

    // Yield to allow the background task to drive the inner service to readiness.
    tokio::task::yield_now().await;

    assert_ready_ok!(service.poll_ready());
    let fut = service.call(());

    let ((), rsp) = handle.next_request().await.expect("must get a request");
    rsp.send_response(());

    let ret = fut.await;
    assert!(ret.is_ok());
}

#[tokio::test]
async fn drains_buffer() {
    use tower::{buffer::Buffer, Layer};

    let _trace = linkerd_tracing::test::with_default_filter("trace");
    tokio::time::pause();

    let max_unavailable = Duration::from_millis(100);
    let (service, mut handle) = mock::pair::<(), ()>();

    let layer = FailFast::layer_gated(max_unavailable, layer::mk(|inner| Buffer::new(inner, 3)));
    let mut service = Spawn::new(layer.layer(service));

    // The inner starts unavailable...
    handle.allow(0);
    // ...but the buffer will accept requests while it has capacity.
    assert_ready_ok!(service.poll_ready());
    let mut buffer1 = task::spawn(service.call(()));

    assert_ready_ok!(service.poll_ready());
    let mut buffer2 = task::spawn(service.call(()));

    assert_ready_ok!(service.poll_ready());
    let mut buffer3 = task::spawn(service.call(()));

    // The buffer is now full
    assert_pending!(service.poll_ready());

    // Then we wait for the idle timeout, at which point failfast should
    // trigger and the buffer requests should be failed.
    tokio::time::sleep(max_unavailable + Duration::from_millis(1)).await;
    // However, the *outer* service should remain unready.
    assert_pending!(service.poll_ready());

    // Buffered requests should now fail.
    assert_ready_err!(buffer1.poll());
    assert_ready_err!(buffer2.poll());
    assert_ready_err!(buffer3.poll());
    drop((buffer1, buffer2, buffer3));

    // The buffer has been drained, but the outer service should still be
    // pending.
    assert_pending!(service.poll_ready());

    // Then the inner service becomes available.
    handle.allow(1);
    tracing::info!("handle.allow(1)");

    // Yield to allow the background task to drive the inner service to readiness.
    tokio::task::yield_now().await;

    assert_ready_ok!(service.poll_ready());
    let fut = service.call(());

    let ((), rsp) = handle.next_request().await.expect("must get a request");
    rsp.send_response(());

    let ret = fut.await;
    assert!(ret.is_ok());
}
