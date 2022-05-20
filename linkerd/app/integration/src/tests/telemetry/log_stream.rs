use super::*;
use futures::future::{self, FutureExt};
use tokio::{sync::oneshot, task::JoinHandle};

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

    let (logs, done) = get_log_stream(metrics, "info,linkerd=debug").await;

    info!("client.get(/)");
    assert_eq!(client.get("/").await, "hello");

    // finish streaming logs so we don't loop forever
    let _ = done.send(());

    let json = logs.await.unwrap();

    assert!(!json.is_empty());

    for obj in json {
        println!("{}\n", obj);
    }
}

#[tokio::test]
async fn query_is_valid_json() {
    let Fixture {
        client,
        metrics,
        proxy: _proxy,
        _profile,
        dst_tx: _dst_tx,
        ..
    } = Fixture::outbound().await;

    let (logs, done) = query_log_stream(metrics, "info,linkerd=debug").await;

    info!("client.get(/)");
    assert_eq!(client.get("/").await, "hello");

    // finish streaming logs so we don't loop forever
    let _ = done.send(());

    let json = logs.await.unwrap();
    assert!(!json.is_empty());

    for obj in json {
        println!("{}\n", obj);
    }
}

#[tokio::test]
async fn valid_get_does_not_error() {
    let Fixture {
        metrics,
        proxy: _proxy,
        _profile,
        dst_tx: _dst_tx,
        ..
    } = Fixture::outbound().await;

    let (logs, done) = get_log_stream(metrics, "info,linkerd=debug").await;

    // finish streaming logs so we don't loop forever
    let _ = done.send(());

    let json = logs.await.unwrap();
    for obj in json {
        println!("{}\n", obj);
        if obj.get("error").is_some() {
            panic!(
                "expected the log stream to contain no error responses!\njson = {}",
                obj
            );
        }
    }
}

#[tokio::test]
async fn valid_query_does_not_error() {
    let Fixture {
        metrics,
        proxy: _proxy,
        _profile,
        dst_tx: _dst_tx,
        ..
    } = Fixture::outbound().await;

    let (logs, done) = query_log_stream(metrics, "info,linkerd=debug").await;

    // finish streaming logs so we don't loop forever
    let _ = done.send(());

    let json = logs.await.unwrap();
    for obj in json {
        println!("{}\n", obj);
        if obj.get("error").is_some() {
            panic!(
                "expected the log stream to contain no error responses!\njson = {}",
                obj
            );
        }
    }
}

#[tokio::test]
async fn multi_filter() {
    let Fixture {
        client,
        metrics,
        proxy,
        _profile,
        dst_tx: _dst_tx,
        ..
    } = Fixture::outbound().await;

    // start streaming the logs
    let (debug_logs, debug_done) = get_log_stream(metrics, "debug").await;
    let (hyper_logs, hyper_done) =
        get_log_stream(client::http1(proxy.admin, "localhost"), "hyper=trace").await;

    info!("client.get(/)");
    assert_eq!(client.get("/").await, "hello");

    // finish streaming logs so we don't loop forever
    let _ = debug_done.send(());
    let _ = hyper_done.send(());

    let json = debug_logs.await.unwrap();
    for obj in json {
        let level = obj.get("level");
        assert!(
            matches!(
                level.and_then(|value| value.as_str()),
                Some("DEBUG") | Some("INFO") | Some("WARN") | Some("ERROR")
            ),
            "level must be DEBUG, INFO, WARN, or ERROR\n level: {:?}\n  json: {:#?}",
            level,
            obj
        );
    }

    let json = hyper_logs.await.unwrap();
    for obj in json {
        let target = obj.get("target").and_then(|value| value.as_str());
        match target {
            Some(s) if s.starts_with("hyper") => {}
            _ => panic!(
                "target must be from a module in `hyper`!\n target: {:?}\n   json: {:#?}",
                obj.get("target"),
                obj
            ),
        }
    }
}

const PATH: &str = "/logs.json";

/// Start a log stream with a GET request
async fn get_log_stream(
    client: client::Client,
    filter: impl ToString,
) -> (JoinHandle<Vec<serde_json::Value>>, oneshot::Sender<()>) {
    let filter = filter.to_string();

    // start the request
    let req = client
        .request_body(
            client
                .request_builder(&format!("{}?{}", PATH, filter))
                .method(http::Method::GET)
                .body(hyper::Body::from(filter))
                .unwrap(),
        )
        .await;
    assert_eq!(req.status(), http::StatusCode::OK);
    // spawn a task to collect and parse all the logs
    collect_logs(req.into_body())
}

/// Start a log stream with a QUERY request
async fn query_log_stream(
    client: client::Client,
    filter: impl ToString,
) -> (JoinHandle<Vec<serde_json::Value>>, oneshot::Sender<()>) {
    let filter = filter.to_string();

    // start the request
    let req = client
        .request_body(
            client
                .request_builder(PATH)
                .method("QUERY")
                .body(hyper::Body::from(filter))
                .unwrap(),
        )
        .await;
    assert_eq!(req.status(), http::StatusCode::OK);
    // spawn a task to collect and parse all the logs
    collect_logs(req.into_body())
}

/// Spawns a task to collect all the logs in a streaming body and parse them as
/// JSON.
fn collect_logs(
    mut body: hyper::Body,
) -> (JoinHandle<Vec<serde_json::Value>>, oneshot::Sender<()>) {
    let (done_tx, done_rx) = oneshot::channel();
    let result = tokio::spawn(async move {
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
    });

    (result, done_tx)
}
