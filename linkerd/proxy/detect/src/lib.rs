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
pub struct DetectProtocolLayer<D> {
    detect: D,
    peek_capacity: usize,
}

#[derive(Debug, Clone)]
pub struct DetectProtocol<D, A> {
    detect: D,
    accept: A,
    peek_capacity: usize,
}

pub enum DetectProtocolFuture<T, D, A>
where
    A: core::listen::Accept<(D::Target, BoxedIo)>,
    D: Detect<T>,
{
    Detected(A::Future),
    Detecting {
        accept: A,
        detect: D,
        inner: PeekAndDetect<T, D>,
    },
}

pub enum PeekAndDetect<T, D: Detect<T>> {
    // Waiting for accept to become ready.
    Detected(Option<(D::Target, BoxedIo)>),
    // Waiting for the prefix to be read.
    Peek(Option<T>, Peek<BoxedIo>),
}

impl<D> DetectProtocolLayer<D> {
    const DEFAULT_CAPACITY: usize = 8192;

    pub fn new(detect: D) -> DetectProtocolLayer<D> {
        DetectProtocolLayer {
            detect,
            peek_capacity: Self::DEFAULT_CAPACITY,
        }
    }
}

impl<D: Clone, A> tower::layer::Layer<A> for DetectProtocolLayer<D> {
    type Service = DetectProtocol<D, A>;

    fn layer(&self, accept: A) -> Self::Service {
        DetectProtocol {
            detect: self.detect.clone(),
            peek_capacity: self.peek_capacity,
            accept,
        }
    }
}

impl<T, D, A> tower::Service<(T, BoxedIo)> for DetectProtocol<D, A>
where
    D: Detect<T>,
    A: core::listen::Accept<(D::Target, BoxedIo)> + Clone,
    D::Target: std::fmt::Debug,
{
    type Response = A::ConnectionFuture;
    type Error = Error;
    type Future = DetectProtocolFuture<T, D, A>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.accept.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, (target, io): (T, BoxedIo)) -> Self::Future {
        match self.detect.detect_before_peek(target) {
            Ok(detected) => DetectProtocolFuture::Detected(self.accept.accept((detected, io))),
            Err(target) => DetectProtocolFuture::Detecting {
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

impl<T, D, A> Future for DetectProtocolFuture<T, D, A>
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
                DetectProtocolFuture::Detected(ref mut fut) => {
                    return fut.poll().map_err(Into::into)
                }
                DetectProtocolFuture::Detecting {
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
                        let inner_fut = accept.accept((target, BoxedIo::new(io)));
                        *self = DetectProtocolFuture::Detected(inner_fut);
                    }

                    PeekAndDetect::Detected(ref mut io) => {
                        try_ready!(accept.poll_ready().map_err(Into::into));
                        let io = io.take().expect("polled after complete");
                        let inner_fut = accept.accept(io);
                        *self = DetectProtocolFuture::Detected(inner_fut);
                    }
                },
            }
        }
    }
}
