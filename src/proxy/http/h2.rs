use std::marker::PhantomData;
use std::{error::Error as StdError, fmt};

use futures::{Future, Poll};
use http;
use hyper::{
    body::Payload,
    client::conn::{self, Handshake, SendRequest},
};

use super::{Body, ClientUsedTls, Error};
use app::config::H2Settings;
use svc;
use task::{ArcExecutor, BoxSendFuture, Executor};
use transport::{connect, tls::HasStatus as HasTlsStatus};

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

pub struct ConnectFuture<C: connect::Connect, B> {
    executor: ArcExecutor,
    state: ConnectState<C, B>,
    h2_settings: H2Settings,
}

enum ConnectState<C: connect::Connect, B> {
    Connect(C::Future),
    Handshake {
        client_used_tls: bool,
        hs: Handshake<C::Connected, B>,
    },
}

pub struct ResponseFuture {
    client_used_tls: bool,
    inner: conn::ResponseFuture,
}

#[derive(Debug)]
pub enum ConnectError<E> {
    Connect(E),
    Handshake(Error),
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
}

impl<C, B> svc::Service<()> for Connect<C, B>
where
    C: connect::Connect,
    C::Connected: HasTlsStatus + Send + 'static,
    B: Payload,
{
    type Response = Connection<B>;
    type Error = ConnectError<C::Error>;
    type Future = ConnectFuture<C, B>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, _target: ()) -> Self::Future {
        ConnectFuture {
            executor: self.executor.clone(),
            state: ConnectState::Connect(self.connect.connect()),
            h2_settings: self.h2_settings,
        }
    }
}

// ===== impl ConnectFuture =====

impl<C, B> Future for ConnectFuture<C, B>
where
    C: connect::Connect,
    C::Connected: HasTlsStatus + Send + 'static,
    B: Payload,
{
    type Item = Connection<B>;
    type Error = ConnectError<C::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let io = match self.state {
                ConnectState::Connect(ref mut fut) => {
                    try_ready!(fut.poll().map_err(ConnectError::Connect))
                }
                ConnectState::Handshake {
                    ref mut hs,
                    client_used_tls,
                } => {
                    let (tx, conn) =
                        try_ready!(hs.poll().map_err(|err| ConnectError::Handshake(err.into())));
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
    type Error = Error;
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
    type Error = Error;

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

// ===== impl ConnectError =====

impl<E: fmt::Display> fmt::Display for ConnectError<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ConnectError::Connect(err) => fmt::Display::fmt(err, f),
            ConnectError::Handshake(err) => fmt::Display::fmt(err, f),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> StdError for ConnectError<E> {}
