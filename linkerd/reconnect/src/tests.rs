use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use super::*;
use futures::prelude::*;
use linkerd_error::recover;
use tokio::time;
use tokio_test::*;
use tower_test::mock::{self, Spawn};

/// Tests that the reconnect layer proxies requests as expected. This test does not exercise
/// reconnects.
#[tokio::test]
async fn proxies_service() {
    let _trace = linkerd_tracing::test::trace_init();

    let (service, mut handle) = mock::pair::<String, String>();
    handle.allow(2);

    let mut service = Spawn::new(Reconnect::new(
        (),
        |()| service.clone(),
        recover::Immediately::default(),
    ));

    assert_ready!(service.poll_ready()).unwrap();
    let ((), rsp) = tokio::join!(
        handle.next_request().map(|req| {
            let (req, rsp) = req.unwrap();
            assert_eq!(req, "hello");
            rsp.send_response("world".to_string());
        }),
        service.call("hello".to_string()),
    );
    assert_eq!(rsp.unwrap(), "world");

    assert_ready!(service.poll_ready()).unwrap();
    let ((), rsp) = tokio::join!(
        handle.next_request().map(|req| {
            let (req, rsp) = req.unwrap();
            assert_eq!(req, "goodbye");
            rsp.send_response("pain".to_string());
        }),
        service.call("goodbye".to_string()),
    );
    assert_eq!(rsp.unwrap(), "pain");
}

/// Tests that the reconnect layer rebuilds the service after poll_ready fails.
#[tokio::test]
async fn reconnect_after_ready() {
    let _trace = linkerd_tracing::test::trace_init();

    let (service, mut handle) = mock::pair::<String, String>();

    let news = Arc::new(AtomicUsize::default());
    let new_service = {
        let news = news.clone();
        move |()| {
            news.fetch_add(1, Ordering::SeqCst);
            service.clone()
        }
    };
    let mut service = Spawn::new(Reconnect::new(
        (),
        new_service,
        recover::Immediately::default(),
    ));

    assert_eq!(news.load(Ordering::SeqCst), 0);

    handle.allow(0);
    handle.send_error("dead");
    assert_pending!(service.poll_ready());
    assert_eq!(news.load(Ordering::SeqCst), 2);

    handle.allow(1);
    assert_ready!(service.poll_ready()).unwrap();
}

/// Tests that the reconnect layer delays rebuilding the inner service for a backoff duration.
#[tokio::test]
async fn backoff() {
    let _trace = linkerd_tracing::test::trace_init();

    let (service, mut handle) = mock::pair::<String, String>();

    let news = Arc::new(AtomicUsize::default());
    let new_service = {
        let news = news.clone();
        move |()| {
            news.fetch_add(1, Ordering::SeqCst);
            service.clone()
        }
    };
    let mut service = Spawn::new(Reconnect::new((), new_service, |e: Error| {
        assert_eq!(e.to_string(), "dead");
        let backoff = time::interval_at(
            time::Instant::now() + time::Duration::from_secs(1),
            time::Duration::from_secs(2),
        );
        Ok(tokio_stream::wrappers::IntervalStream::new(backoff).map(|_| ()))
    }));

    time::pause();

    handle.allow(0);
    handle.send_error("dead");
    assert_eq!(news.load(Ordering::SeqCst), 0);
    assert_pending!(service.poll_ready());
    assert_eq!(news.load(Ordering::SeqCst), 1);

    // After the first failure, we backoff for 1s; so sleep for only half that time to ensure it
    // hasn't been recreated.
    time::sleep(time::Duration::from_millis(500)).await;
    assert_pending!(service.poll_ready());
    assert_eq!(news.load(Ordering::SeqCst), 1);

    // After the backoff completes, we should have created a new service.
    time::sleep(time::Duration::from_millis(500)).await;
    assert_pending!(service.poll_ready());
    assert_eq!(news.load(Ordering::SeqCst), 2);

    // After the second failure, we backoff for 2s; so sleep for only half that time to ensure it
    // hasn't been recreated.
    handle.send_error("dead");
    assert_pending!(service.poll_ready());
    time::sleep(time::Duration::from_secs(1)).await;
    assert_pending!(service.poll_ready());
    assert_eq!(news.load(Ordering::SeqCst), 2);

    // After the backoff completes, we should have created a new service.
    time::sleep(time::Duration::from_secs(1)).await;
    assert_pending!(service.poll_ready());
    assert_eq!(news.load(Ordering::SeqCst), 3);

    // Then the service should become ready.
    handle.allow(1);
    assert_ready!(service.poll_ready()).unwrap();
    assert_eq!(news.load(Ordering::SeqCst), 3);

    // On the next failure, the backoff is reset.
    handle.send_error("dead");
    assert_pending!(service.poll_ready());
    time::sleep(time::Duration::from_secs(1)).await;
    assert_ready!(service.poll_ready()).unwrap();
    assert_eq!(news.load(Ordering::SeqCst), 4);
}
