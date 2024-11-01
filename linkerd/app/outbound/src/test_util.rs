use crate::Config;
pub use futures::prelude::*;
use linkerd_app_core::{
    config::{self, QueueConfig},
    drain, exp_backoff, metrics,
    proxy::{
        http::{h1, h2},
        tap,
    },
    transport::{DualListenAddr, Keepalive, UserTimeout},
    IpMatch, IpNet, ProxyRuntime,
};
pub use linkerd_app_test as support;
use std::{str::FromStr, time::Duration};

pub(crate) fn default_config() -> Config {
    let buffer = QueueConfig {
        capacity: 10_000,
        failfast_timeout: Duration::from_secs(3),
    };
    Config {
        ingress_mode: false,
        emit_headers: true,
        allow_discovery: IpMatch::new(Some(IpNet::from_str("0.0.0.0/0").unwrap())).into(),
        proxy: config::ProxyConfig {
            server: config::ServerConfig {
                addr: DualListenAddr(([0, 0, 0, 0], 0).into(), None),
                keepalive: Keepalive(None),
                user_timeout: UserTimeout(None),
                http2: h2::ServerParams::default(),
            },
            connect: config::ConnectConfig {
                keepalive: Keepalive(None),
                user_timeout: UserTimeout(None),
                timeout: Duration::from_secs(1),
                backoff: exp_backoff::ExponentialBackoff::try_new(
                    Duration::from_millis(100),
                    Duration::from_millis(500),
                    0.1,
                )
                .unwrap(),
                http1: h1::PoolSettings {
                    max_idle: 1,
                    idle_timeout: Duration::from_secs(1),
                },
                http2: h2::ClientParams::default(),
            },
            max_in_flight_requests: 10_000,
            detect_protocol_timeout: Duration::from_secs(3),
        },
        inbound_ips: Default::default(),
        discovery_idle_timeout: Duration::from_secs(60),
        tcp_connection_queue: buffer,
        http_request_queue: buffer,
    }
}

pub(crate) fn runtime() -> (ProxyRuntime, drain::Signal) {
    let (drain_tx, drain) = drain::channel();
    let (tap, _) = tap::new();
    let (metrics, _) = metrics::Metrics::new(std::time::Duration::from_secs(10));
    let runtime = ProxyRuntime {
        identity: linkerd_meshtls_rustls::creds::default_for_test().1.into(),
        metrics: metrics.proxy,
        tap,
        span_sink: None,
        drain,
    };
    (runtime, drain_tx)
}

pub use self::mock_body::MockBody;

mod mock_body {
    use bytes::Bytes;
    use linkerd_app_core::proxy::http::HttpBody;
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

        /// Returns a [`MockBody`] that never yields any data.
        pub fn pending() -> Self {
            let fut = futures::future::pending();
            Self::new(fut)
        }

        /// Returns a [`MockBody`] that yields an error when polled.
        pub fn error(msg: &'static str) -> Self {
            let err = Err(msg.into());
            let fut = futures::future::ready(err);
            Self::new(fut)
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
        ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
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
