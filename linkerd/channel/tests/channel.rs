#![warn(rust_2018_idioms)]
use linkerd2_channel::{channel, error::TrySendError, Receiver, Sender};

use std::sync::Arc;
use tokio_test::task;
use tokio_test::{assert_err, assert_ok, assert_pending, assert_ready, assert_ready_ok};

trait AssertSend: Send {}
impl AssertSend for Sender<i32> {}
impl AssertSend for Receiver<i32> {}

#[tokio::test]
async fn send_recv_with_buffer() {
    let (mut tx, mut rx) = channel::<i32>(16);

    // Using poll_ready / try_send
    assert_ready_ok!(task::spawn(tx.ready()).poll());
    tx.try_send(1).unwrap();

    // Without poll_ready
    tx.send(2).await.unwrap();

    drop(tx);

    let val = rx.recv().await;
    assert_eq!(val, Some(1));

    let val = rx.recv().await;
    assert_eq!(val, Some(2));

    let val = rx.recv().await;
    assert!(val.is_none());
}

#[tokio::test]
async fn ready_disarm() {
    let (tx, mut rx) = channel::<i32>(2);
    let mut tx1 = tx.clone();
    let mut tx2 = tx.clone();
    let mut tx3 = tx.clone();
    let mut tx4 = tx;

    // We should be able to `poll_ready` two handles without problem
    let _ = assert_ok!(tx1.ready().await);
    let _ = assert_ok!(tx2.ready().await);

    // But a third should not be ready
    let mut r3 = task::spawn(tx3.ready());
    assert_pending!(r3.poll());

    let mut r4 = task::spawn(tx4.ready());
    assert_pending!(r4.poll());

    // Using one of the readyd slots should allow a new handle to become ready
    tx1.send(1).await.unwrap();

    // We also need to receive for the slot to be free
    assert!(!r3.is_woken());
    rx.recv().await.unwrap();
    // Now there's a free slot!
    assert!(r3.is_woken());
    assert!(!r4.is_woken());

    // Dropping a permit should also open up a slot
    drop(tx2);
    assert!(r4.is_woken());

    let mut r1 = task::spawn(tx1.ready());
    assert_pending!(r1.poll());
}

#[tokio::test]
async fn send_recv_stream_with_buffer() {
    use tokio::stream::StreamExt;

    let (mut tx, mut rx) = channel::<i32>(16);

    tokio::spawn(async move {
        assert_ok!(tx.send(1).await);
        assert_ok!(tx.send(2).await);
    });

    assert_eq!(Some(1), rx.next().await);
    assert_eq!(Some(2), rx.next().await);
    assert_eq!(None, rx.next().await);
}

#[tokio::test]
async fn async_send_recv_with_buffer() {
    let (mut tx, mut rx) = channel(16);

    tokio::spawn(async move {
        assert_ok!(tx.send(1).await);
        assert_ok!(tx.send(2).await);
    });

    assert_eq!(Some(1), rx.recv().await);
    assert_eq!(Some(2), rx.recv().await);
    assert_eq!(None, rx.recv().await);
}

#[tokio::test]
async fn start_send_past_cap() {
    let (mut tx1, mut rx) = channel(1);
    let mut tx2 = tx1.clone();

    assert_ok!(tx1.try_send(()));

    let mut r1 = task::spawn(tx1.ready());
    assert_pending!(r1.poll());

    {
        let mut r2 = task::spawn(tx2.ready());
        assert_pending!(r2.poll());

        drop(r1);
        drop(tx1);

        assert!(rx.recv().await.is_some());

        assert!(r2.is_woken());
    }

    drop(tx2);

    assert!(rx.recv().await.is_none());
}

#[test]
#[should_panic]
fn buffer_gteq_one() {
    channel::<i32>(0);
}

#[tokio::test]
async fn no_t_bounds_buffer() {
    struct NoImpls;

    let (tx, mut rx) = channel(100);

    // sender should be Debug even though T isn't Debug
    println!("{:?}", tx);
    // same with Receiver
    println!("{:?}", rx);
    // and sender should be Clone even though T isn't Clone
    assert!(tx.clone().send(NoImpls).await.is_ok());

    assert!(rx.recv().await.is_some());
}

#[tokio::test]
async fn send_recv_buffer_limited() {
    let (mut tx, mut rx) = channel::<i32>(1);

    // ready capacity
    assert_ok!(tx.ready().await);

    // Send first message
    tx.try_send(1).unwrap();

    // Not ready
    let mut p2 = task::spawn(tx.ready());
    assert_pending!(p2.poll());

    // Take the value
    assert!(rx.recv().await.is_some());

    // Notified
    assert!(p2.is_woken());

    // Send second
    assert_ready_ok!(p2.poll());
    drop(p2);
    tx.try_send(2).unwrap();

    assert!(rx.recv().await.is_some());
}

#[tokio::test]
async fn tx_close_gets_none() {
    let (_, mut rx) = channel::<i32>(10);
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn try_send_fail() {
    let (mut tx, mut rx) = channel(1);

    tx.ready().await.unwrap();
    tx.try_send("hello").unwrap();

    // This should fail
    match assert_err!(tx.try_send("fail")) {
        TrySendError::Full(..) => {}
        _ => panic!(),
    }

    assert_eq!(rx.recv().await, Some("hello"));

    assert_ok!(tx.try_send("goodbye"));
    drop(tx);

    assert_eq!(rx.recv().await, Some("goodbye"));
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn drop_tx_releases_permit() {
    // ready reserves a permit capacity, ensure that the capacity is
    // released if tx is dropped w/o sending a value.
    let (mut tx1, _rx) = channel::<i32>(1);
    let mut tx2 = tx1.clone();

    assert_ok!(tx1.ready().await);

    let mut ready2 = task::spawn(tx2.ready());
    assert_pending!(ready2.poll());

    drop(tx1);

    assert!(ready2.is_woken());
    assert_ready_ok!(ready2.poll());
}

#[tokio::test]
async fn dropping_rx_closes_channel() {
    let (mut tx, rx) = channel(100);

    let msg = Arc::new(());
    assert_ok!(tx.try_send(msg.clone()));

    drop(rx);
    assert_err!(tx.ready().await);
    assert_eq!(1, Arc::strong_count(&msg));
}

#[test]
fn dropping_rx_closes_channel_for_try() {
    let (mut tx, rx) = channel(100);

    let msg = Arc::new(());
    tx.try_send(msg.clone()).unwrap();

    drop(rx);

    {
        let err = assert_err!(tx.try_send(msg.clone()));
        match err {
            TrySendError::Closed(..) => {}
            _ => panic!(),
        }
    }

    assert_eq!(1, Arc::strong_count(&msg));
}

#[test]
fn unconsumed_messages_are_dropped() {
    let msg = Arc::new(());

    let (mut tx, rx) = channel(100);

    tx.try_send(msg.clone()).unwrap();

    assert_eq!(2, Arc::strong_count(&msg));

    drop((tx, rx));

    assert_eq!(1, Arc::strong_count(&msg));
}
