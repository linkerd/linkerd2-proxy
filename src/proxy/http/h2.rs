use std::{fmt, error::Error as StdError};
use std::marker::PhantomData;

use futures::{Future, Poll};
use http;
use hyper::{body::Payload, client::conn::{self, Handshake, SendRequest}};

use super::{Body, Error};
use svc;
use task::{ArcExecutor, BoxSendFuture, Executor};
use transport::connect;

#[derive(Debug)]
pub struct Connect<C, B> {
    connect: C,
    executor: ArcExecutor,
    _marker: PhantomData<fn() -> B>,
}

#[derive(Debug)]
pub struct Connection<B> {
    tx: SendRequest<B>,
}

pub struct ConnectFuture<C: connect::Connect, B> {
    executor: ArcExecutor,
    state: ConnectState<C, B>,
}

enum ConnectState<C: connect::Connect, B> {
    Connect(C::Future),
    Handshake(Handshake<C::Connected, B>)
}

pub struct ResponseFuture(conn::ResponseFuture);

#[derive(Debug)]
pub enum ConnectError<E> {
    Connect(E),
    Handshake(Error),
}

// ===== impl Connect =====

impl<C, B> Connect<C, B> {
    pub fn new<E>(connect: C, executor: E) -> Self
    where
        E: Executor<BoxSendFuture> + Clone + Send + Sync + 'static,
    {
        Connect {
            connect,
            executor: ArcExecutor::new(executor),
            _marker: PhantomData,
        }
    }
}

impl<C, B> svc::Service<()> for Connect<C, B>
where
    C: connect::Connect,
    C::Connected: Send + 'static,
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
        }
    }
}

// ===== impl ConnectFuture =====

impl<C, B> Future for ConnectFuture<C, B>
where
    C: connect::Connect,
    C::Connected: Send + 'static,
    B: Payload,
{
    type Item = Connection<B>;
    type Error = ConnectError<C::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let io = match self.state {
                ConnectState::Connect(ref mut fut) => {
                    try_ready!(fut.poll().map_err(ConnectError::Connect))
                },
                ConnectState::Handshake(ref mut fut) => {
                    let (tx, conn) = try_ready!(fut.poll().map_err(|err| ConnectError::Handshake(err.into())));
                    let _ = self
                        .executor
                        .execute(conn.map_err(|err| debug!("http2 conn error: {}", err)));

                    return Ok(Connection { tx }.into());
                }
            };

            let handshake = conn::Builder::new()
                .executor(self.executor.clone())
                .http2_only(true)
                .handshake(io);
            self.state = ConnectState::Handshake(handshake);
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
        ResponseFuture(self.tx.send_request(req))
    }
}

// ===== impl ResponseFuture =====

impl Future for ResponseFuture {
    type Item = http::Response<Body>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let res = try_ready!(self.0.poll());
        let res = res.map(|body| Body {
            body: Some(body),
            upgrade: None,
        });
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

impl<E: StdError> StdError for ConnectError<E> {}

