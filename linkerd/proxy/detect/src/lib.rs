/* use futures::{try_ready, Future, Poll}; */
use futures::compat::{Compat01As03, Future01CompatExt};
use linkerd2_error::Error;
use linkerd2_io::{BoxedIo, Peek};
use linkerd2_proxy_core as core;
use pin_project::{pin_project, project};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A strategy for detecting values out of a client transport.
pub trait Detect<T>: Clone {
    type Target;

    /// If the target can be determined by the target alone (i.e. because it's
    /// known to be a server-speaks-first target), Otherwise, the target is
    /// returned as an error.
    fn detect_before_peek(&self, target: T) -> Result<Self::Target, T>;

    /// If the target could not be determined without peeking, then used the
    /// peeked prefix to determine the protocol.
    fn detect_peeked_prefix(&self, target: T, prefix: &[u8]) -> Self::Target;
}

#[derive(Debug, Clone)]
pub struct Accept<D, A> {
    detect: D,
    accept: A,
    peek_capacity: usize,
}

#[pin_project]
pub struct AcceptFuture<T, D, A>
where
    D: Detect<T>,
    A: core::listen::Accept<(D::Target, BoxedIo)>,
{
    #[pin]
    state: State<T, D, A>,
}

#[pin_project]
enum State<T, D, A>
where
    D: Detect<T>,
    A: core::listen::Accept<(D::Target, BoxedIo)>,
{
    Accept(#[pin] A::Future),
    Detect {
        detect: D,
        accept: A,
        #[pin]
        inner: PeekAndDetect<T, D>,
    },
}

#[pin_project]
pub enum PeekAndDetect<T, D: Detect<T>> {
    // Waiting for accept to become ready.
    Detected(Option<(D::Target, BoxedIo)>),
    // Waiting for the prefix to be read.
    Peek(Option<T>, #[pin] Compat01As03<Peek<BoxedIo>>),
}

impl<D, A> Accept<D, A> {
    const DEFAULT_CAPACITY: usize = 8192;

    /// Creates a new `Detect`.
    pub fn new(detect: D, accept: A) -> Self {
        Self {
            detect,
            accept,
            peek_capacity: Self::DEFAULT_CAPACITY,
        }
    }

    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.peek_capacity = capacity;
        self
    }
}

impl<T, D, A> tower::Service<(T, BoxedIo)> for Accept<D, A>
where
    D: Detect<T>,
    A: core::listen::Accept<(D::Target, BoxedIo)> + Clone,
{
    type Response = ();
    type Error = Error;
    type Future = AcceptFuture<T, D, A>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.accept.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, (target, io): (T, BoxedIo)) -> Self::Future {
        match self.detect.detect_before_peek(target) {
            Ok(detected) => AcceptFuture {
                state: State::Accept(self.accept.accept((detected, io))),
            },
            Err(target) => AcceptFuture {
                state: State::Detect {
                    detect: self.detect.clone(),
                    accept: self.accept.clone(),
                    inner: PeekAndDetect::Peek(
                        Some(target),
                        Peek::with_capacity(self.peek_capacity, io).compat(),
                    ),
                },
            },
        }
    }
}

impl<T, D, A> Future for AcceptFuture<T, D, A>
where
    D: Detect<T>,
    A: core::listen::Accept<(D::Target, BoxedIo)>,
    A::Error: Into<Error>,
{
    type Output = Result<(), Error>;

    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        loop {
            #[project]
            match this.state.as_mut().project() {
                State::Accept(fut) => return fut.poll(cx).map_err(Into::into),
                State::Detect {
                    detect,
                    accept,
                    mut inner,
                } =>
                {
                    #[project]
                    match inner.as_mut().project() {
                        PeekAndDetect::Peek(target, peek) => {
                            let io = futures::ready!(peek.poll(cx))?;
                            let target = detect.detect_peeked_prefix(
                                target.take().expect("polled after complete"),
                                io.prefix().as_ref(),
                            );
                            inner.set(PeekAndDetect::Detected(Some((target, BoxedIo::new(io)))));
                        }
                        PeekAndDetect::Detected(io) => {
                            futures::ready!(accept.poll_ready(cx)).map_err(Into::into)?;
                            let io = io.take().expect("polled after complete");
                            let accept = accept.accept(io);
                            this.state.set(State::Accept(accept));
                        }
                    }
                }
            }
        }
    }
}
