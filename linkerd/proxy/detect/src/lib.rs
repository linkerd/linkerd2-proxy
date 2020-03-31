use futures::{try_ready, Future, Poll};
use linkerd2_error::Error;
use linkerd2_io::{BoxedIo, Peek};
use linkerd2_proxy_core as core;

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

pub enum AcceptFuture<T, D, A>
where
    D: Detect<T>,
    A: core::listen::Accept<(D::Target, BoxedIo)>,
{
    Accept(A::Future),
    Detect {
        detect: D,
        accept: A,
        inner: PeekAndDetect<T, D>,
    },
}

pub enum PeekAndDetect<T, D: Detect<T>> {
    // Waiting for accept to become ready.
    Detected(Option<(D::Target, BoxedIo)>),
    // Waiting for the prefix to be read.
    Peek(Option<T>, Peek<BoxedIo>),
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
    D::Target: std::fmt::Debug,
{
    type Response = A::ConnectionFuture;
    type Error = Error;
    type Future = AcceptFuture<T, D, A>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.accept.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, (target, io): (T, BoxedIo)) -> Self::Future {
        match self.detect.detect_before_peek(target) {
            Ok(detected) => AcceptFuture::Accept(self.accept.accept((detected, io))),
            Err(target) => AcceptFuture::Detect {
                detect: self.detect.clone(),
                accept: self.accept.clone(),
                inner: PeekAndDetect::Peek(
                    Some(target),
                    Peek::with_capacity(self.peek_capacity, io),
                ),
            },
        }
    }
}

impl<T, D, A> Future for AcceptFuture<T, D, A>
where
    D: Detect<T>,
    D::Target: std::fmt::Debug,
    A: core::listen::Accept<(D::Target, BoxedIo)>,
{
    type Item = A::ConnectionFuture;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self {
                AcceptFuture::Accept(ref mut fut) => return fut.poll().map_err(Into::into),
                AcceptFuture::Detect {
                    ref detect,
                    ref mut accept,
                    ref mut inner,
                } => match inner {
                    PeekAndDetect::Peek(ref mut target, ref mut peek) => {
                        let io = try_ready!(peek.poll().map_err(Error::from));
                        let target = detect.detect_peeked_prefix(
                            target.take().expect("polled after complete"),
                            io.prefix().as_ref(),
                        );
                        *inner = PeekAndDetect::Detected(Some((target, BoxedIo::new(io))));
                    }
                    PeekAndDetect::Detected(ref mut io) => {
                        try_ready!(accept.poll_ready().map_err(Into::into));
                        let io = io.take().expect("polled after complete");
                        *self = AcceptFuture::Accept(accept.accept(io));
                    }
                },
            }
        }
    }
}
