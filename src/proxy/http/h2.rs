use std::marker::PhantomData;

use futures::{Future, Poll};
use http;
use hyper::{
    body::Payload,
    client::conn::{self, Handshake, SendRequest},
};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{Body, ClientUsedTls};
use app::config::H2Settings;
use proxy::Error;
use svc;
use task::{ArcExecutor, BoxSendFuture, Executor};
use transport::tls::HasStatus as HasTlsStatus;

#[derive(Debug)]
pub struct Connect<C, B> {
    connect: C,
    executor: ArcExecutor,
    h2_settings: H2Settings,
    _marker: PhantomData<fn() -> B>,
}

#[derive(Debug)]
pub struct Connection<B> {
    client_used_tls: bool,
    tx: SendRequest<B>,
}

pub struct ConnectFuture<F: Future, B> {
    executor: ArcExecutor,
    state: ConnectState<F, B>,
    h2_settings: H2Settings,
}

enum ConnectState<F: Future, B> {
    Connect(F),
    Handshake {
        client_used_tls: bool,
        hs: Handshake<F::Item, B>,
    },
}

pub struct ResponseFuture {
    client_used_tls: bool,
    inner: conn::ResponseFuture,
}

// ===== impl Connect =====

impl<C, B> Connect<C, B> {
    pub fn new<E>(connect: C, executor: E, h2_settings: H2Settings) -> Self
    where
        E: Executor<BoxSendFuture> + Clone + Send + Sync + 'static,
    {
        Connect {
            connect,
            executor: ArcExecutor::new(executor),
            h2_settings,
            _marker: PhantomData,
        }
    }

    pub fn set_executor<E>(&mut self, executor: E)
    where
        E: Executor<BoxSendFuture> + Clone + Send + Sync + 'static,
    {
        self.executor = ArcExecutor::new(executor);
    }
}

impl<C: Clone, B> Clone for Connect<C, B> {
    fn clone(&self) -> Self {
        Connect {
            connect: self.connect.clone(),
            executor: self.executor.clone(),
            h2_settings: self.h2_settings.clone(),
            _marker: PhantomData,
        }
    }
}

impl<C, B, Target> svc::Service<Target> for Connect<C, B>
where
    C: svc::MakeConnection<Target>,
    C::Connection: HasTlsStatus + Send + 'static,
    C::Error: Into<Error>,
    B: Payload,
{
    type Response = Connection<B>;
    type Error = Error;
    type Future = ConnectFuture<C::Future, B>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.connect.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: Target) -> Self::Future {
        ConnectFuture {
            executor: self.executor.clone(),
            state: ConnectState::Connect(self.connect.make_connection(target)),
            h2_settings: self.h2_settings,
        }
    }
}

// ===== impl ConnectFuture =====

impl<F, B> Future for ConnectFuture<F, B>
where
    F: Future,
    F::Item: HasTlsStatus + AsyncRead + AsyncWrite + Send + 'static,
    F::Error: Into<Error>,
    B: Payload,
{
    type Item = Connection<B>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let io = match self.state {
                ConnectState::Connect(ref mut fut) => try_ready!(fut.poll().map_err(Into::into)),
                ConnectState::Handshake {
                    ref mut hs,
                    client_used_tls,
                } => {
                    let (tx, conn) = try_ready!(hs.poll());
                    let _ = self
                        .executor
                        .execute(conn.map_err(|err| debug!("http2 conn error: {}", err)));

                    return Ok(Connection {
                        client_used_tls,
                        tx,
                    }
                    .into());
                }
            };

            let client_used_tls = io.tls_status().is_some();

            let hs = conn::Builder::new()
                .executor(self.executor.clone())
                .http2_only(true)
                .http2_initial_stream_window_size(self.h2_settings.initial_stream_window_size)
                .http2_initial_connection_window_size(
                    self.h2_settings.initial_connection_window_size,
                )
                .handshake(io);
            self.state = ConnectState::Handshake {
                client_used_tls,
                hs,
            }
        }
    }
}

// ===== impl Connection =====

impl<B> svc::Service<http::Request<B>> for Connection<B>
where
    B: Payload,
{
    type Response = http::Response<Body>;
    type Error = hyper::Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.tx.poll_ready().map_err(From::from)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        ResponseFuture {
            client_used_tls: self.client_used_tls,
            inner: self.tx.send_request(req),
        }
    }
}

// ===== impl ResponseFuture =====

impl Future for ResponseFuture {
    type Item = http::Response<Body>;
    type Error = hyper::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let res = try_ready!(self.inner.poll());
        let mut res = res.map(|body| Body {
            body: Some(body),
            upgrade: None,
        });
        if self.client_used_tls {
            res.extensions_mut().insert(ClientUsedTls(()));
        }
        Ok(res.into())
    }
}
