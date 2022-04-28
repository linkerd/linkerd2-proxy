use super::*;
use futures::future::{self, FutureExt};
use tokio::sync::oneshot;

#[tokio::test]
async fn is_valid_json() {
    let Fixture {
        client,
        metrics,
        proxy: _proxy,
        _profile,
        dst_tx: _dst_tx,
        ..
    } = Fixture::outbound().await;

    let body = metrics
        .request_body(
            metrics
                .request_builder("/logs")
                .method(http::Method::GET)
                .body(hyper::Body::from("info,linkerd=debug"))
                .unwrap(),
        )
        .await
        .into_body();
    let (done_tx, done_rx) = oneshot::channel();
    let logs = tokio::spawn(stream_logs(body, done_rx));

    info!("client.get(/)");
    assert_eq!(client.get("/").await, "hello");

    // finish streaming logs so we don't loop forever
    let _ = done_tx.send(());

    let json = logs.await.unwrap();
    for obj in json {
        println!("{}\n", obj);
    }
}

async fn stream_logs(
    mut body: hyper::Body,
    done_rx: oneshot::Receiver<()>,
) -> Vec<serde_json::Value> {
    let mut result = Vec::new();
    let logs = &mut result;
    let fut = async move {
        while let Some(res) = body.data().await {
            let chunk = match res {
                Ok(chunk) => chunk,
                Err(e) => {
                    println!("body failed: {}", e);
                    break;
                }
            };
            let deserialized = serde_json::from_slice(&chunk[..]);
            tracing::info!(?deserialized);
            match deserialized {
                Ok(json) => logs.push(json),
                Err(error) => panic!(
                    "parsing logs as JSON failed\n  error: {error}\n  chunk: {:?}",
                    String::from_utf8_lossy(&chunk[..])
                ),
            }
        }
    };
    future::select(Box::pin(fut), done_rx.map(|_| ())).await;

    result
}
