use super::*;
use bytes::Bytes;
use futures::FutureExt;
use http_body_util::BodyExt;
use linkerd_io as io;
use linkerd_stack::CloneParam;
use std::vec;
use tokio::time;
use tower::ServiceExt;
use tower_test::mock;
use tracing::info_span;

/// Tests how the server behaves when the client connection window is exhausted.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn h2_connection_window_exhaustion() {
    let _trace = linkerd_tracing::test::with_default_filter(LOG_LEVEL);

    // Setup a HTTP/2 server with consumers and producers that are mocked for
    // tests.
    const CONCURRENCY: u32 = 3;
    const CLIENT_STREAM_WINDOW: u32 = 65535;
    const CLIENT_CONN_WINDOW: u32 = CONCURRENCY * CLIENT_STREAM_WINDOW;

    tracing::info!("Connecting to server");
    let mut server = TestServer::connect_h2(
        // A basic HTTP/2 server configuration with no overrides.
        h2::ServerParams::default(),
        // An HTTP/2 client with constrained connection and stream windows to
        // force window exhaustion.
        hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .initial_connection_window_size(CLIENT_CONN_WINDOW)
            .initial_stream_window_size(CLIENT_STREAM_WINDOW)
            .timer(hyper_util::rt::TokioTimer::new()),
    )
    .await;

    // Mocked response data to fill up the stream and connection windows.
    let bytes = (0..CLIENT_STREAM_WINDOW).map(|_| b'a').collect::<Bytes>();

    // Response bodies held to exhaust connection window.
    let mut retain = vec![];

    tracing::info!(
        streams = CONCURRENCY - 1,
        data = bytes.len(),
        "Consuming connection window"
    );
    for _ in 0..CONCURRENCY - 1 {
        let rx = timeout(server.respond(bytes.clone()))
            .await
            .expect("timed out");
        retain.push(rx);
    }

    tracing::info!("Processing a stream with available connection window");
    let rx = timeout(server.respond(bytes.clone()))
        .await
        .expect("timed out");
    let body = timeout(rx.collect().instrument(info_span!("collect")))
        .await
        .expect("response timed out")
        .expect("response");
    assert_eq!(body.to_bytes(), bytes);

    tracing::info!("Consuming the remaining connection window");
    let rx = timeout(server.respond(bytes.clone()))
        .await
        .expect("timed out");
    retain.push(rx);

    tracing::info!("The connection window is exhausted");

    tracing::info!("Trying to process an additional stream. The response headers are received but no data is received.");
    let mut rx = timeout(server.respond(bytes.clone()))
        .await
        .expect("timed out");
    tokio::select! {
        _ = time::sleep(time::Duration::from_secs(2)) => {}
        _ = rx.frame() => panic!("unexpected data"),
    }

    tracing::info!("Dropping one of the retained response bodies frees capacity so that the data can be received");
    drop(retain.pop());
    let body = timeout(rx.collect().instrument(info_span!("collect")))
        .await
        .expect("response timed out")
        .expect("response");
    assert_eq!(body.to_bytes(), bytes);
}

/// Tests how the server behaves when the client stream window is exhausted.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn h2_stream_window_exhaustion() {
    let _trace = linkerd_tracing::test::with_default_filter(LOG_LEVEL);

    // Setup a HTTP/2 server with consumers and producers that are mocked for
    // tests.
    const CLIENT_STREAM_WINDOW: u32 = 1024;

    let mut server = TestServer::connect_h2(
        // A basic HTTP/2 server configuration with no overrides.
        h2::ServerParams::default(),
        // An HTTP/2 client with stream windows to force window exhaustion.
        hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .initial_stream_window_size(CLIENT_STREAM_WINDOW)
            .timer(hyper_util::rt::TokioTimer::new()),
    )
    .await;

    let (mut tx, mut body) = timeout(server.get()).await.expect("timed out");

    let chunk = (0..CLIENT_STREAM_WINDOW).map(|_| b'a').collect::<Bytes>();
    tracing::info!(sz = chunk.len(), "Sending chunk");
    tx.send_data(chunk.clone()).await.expect("can send data");
    tokio::task::yield_now().await;

    tracing::info!(sz = chunk.len(), "Buffering chunk in channel");
    tx.send_data(chunk.clone()).await.expect("can send data");
    tokio::task::yield_now().await;

    tracing::info!(sz = chunk.len(), "Confirming stream window exhaustion");
    /*
     * XXX(kate): this can be reinstate when we have a `poll_ready(cx)` method on the new sender.
    assert!(
        timeout(futures::future::poll_fn(|cx| tx.poll_ready(cx)))
            .await
            .is_err(),
        "stream window should be exhausted"
    );
    */

    tracing::info!("Once the pending data is read, the stream window should be replenished");
    let data = body
        .frame()
        .await
        .expect("yields a result")
        .expect("yields a frame")
        .into_data()
        .expect("yields data");
    assert_eq!(data, chunk);
    let data = body
        .frame()
        .await
        .expect("yields a result")
        .expect("yields a frame")
        .into_data()
        .expect("yields data");
    assert_eq!(data, chunk);

    timeout(body.frame()).await.expect_err("no more chunks");

    tracing::info!(sz = chunk.len(), "Confirming stream window availability");
    /*
     * XXX(kate): this can be reinstated when we have a `poll_ready(cx)` method on the new sender.
    timeout(futures::future::poll_fn(|cx| tx.poll_ready(cx)))
        .await
        .expect("timed out")
        .expect("ready");
    */
}

// === Utilities ===

const LOG_LEVEL: &str = "h2::proto=trace,hyper=trace,linkerd=trace,info";

struct TestServer {
    client: hyper::client::conn::http2::SendRequest<BoxBody>,
    server: Handle,
}

type Mock = mock::Mock<http::Request<BoxBody>, http::Response<BoxBody>>;
type Handle = mock::Handle<http::Request<BoxBody>, http::Response<BoxBody>>;

/// Allows us to configure a server from the Params type.
#[derive(Clone, Debug)]
struct NewMock(mock::Mock<http::Request<BoxBody>, http::Response<BoxBody>>);

impl NewService<()> for NewMock {
    type Service = NewMock;
    fn new_service(&self, _: ()) -> Self::Service {
        self.clone()
    }
}

impl NewService<ClientHandle> for NewMock {
    type Service = Mock;
    fn new_service(&self, _: ClientHandle) -> Self::Service {
        self.0.clone()
    }
}

fn drain() -> drain::Watch {
    let (mut sig, drain) = drain::channel();
    tokio::spawn(async move {
        sig.closed().await;
    });
    drain
}

async fn timeout<F: Future>(inner: F) -> Result<F::Output, time::error::Elapsed> {
    time::timeout(time::Duration::from_secs(2), inner).await
}

impl TestServer {
    #[tracing::instrument(skip_all)]
    async fn connect_h2(
        h2: h2::ServerParams,
        client: &mut hyper::client::conn::http2::Builder<TokioExecutor>,
    ) -> Self {
        let params = Params {
            drain: drain(),
            version: Variant::H2,
            http2: h2,
        };

        let (sio, cio) = io::duplex(20 * 1024 * 1024); // 20 MB

        // Build the HTTP server with a mocked inner service so that we can handle
        // requests.
        let (mock, server) = mock::pair::<http::Request<BoxBody>, http::Response<BoxBody>>();
        let svc = NewServeHttp::new(CloneParam::from(params), NewMock(mock)).new_service(());
        fn bound<S: Service<I>, I>(_: &S) {}
        bound::<ServeHttp<NewMock>, linkerd_io::DuplexStream>(&svc);
        let fut = svc.oneshot(sio).instrument(info_span!("server"));
        tokio::spawn(fut);

        // Build a real HTTP/2 client using the mocked socket.
        let (client, task) = client
            .handshake::<_, BoxBody>(hyper_util::rt::tokio::TokioIo::new(cio))
            .await
            .expect("client connect");
        tokio::spawn(task.instrument(info_span!("client")));

        Self { client, server }
    }

    /// Issues a request through the client to the mocked server and processes the
    /// response. The mocked response body sender and the readable response body are
    /// returned.
    #[tracing::instrument(skip(self))]
    async fn get(
        &mut self,
    ) -> (
        http_body_util::channel::Sender<Bytes>,
        hyper::body::Incoming,
    ) {
        self.server.allow(1);
        let mut call0 = self
            .client
            .send_request(http::Request::new(BoxBody::default()))
            .boxed();
        let (_req, next) = tokio::select! {
            _ = (&mut call0) => unreachable!("client cannot receive a response"),
            next = self.server.next_request() => next.expect("server not dropped"),
        };
        let (tx, rx) = http_body_util::channel::Channel::new(512);
        next.send_response(http::Response::new(BoxBody::new(rx)));
        let rsp = call0.await.expect("response");
        (tx, rsp.into_body())
    }

    #[tracing::instrument(skip(self))]
    async fn respond(&mut self, body: Bytes) -> hyper::body::Incoming {
        let (mut tx, rx) = self.get().await;
        tx.send_data(body.clone()).await.expect("send data");
        rx
    }
}
