use futures::{try_ready, Async, Future, Poll};

use linkerd2_error::Error;
use linkerd2_io::{BoxedIo, Peek};

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

pub enum PeekAndDetect<T, D: Detect<T>> {
    // Waiting for the inner svc to become ready.
    Detected(Option<(D::Target, BoxedIo)>),
    // Waiting for the prefix to be read.
    Peek(Option<T>, Peek<BoxedIo>),
}

#[derive(Debug, Clone)]
pub struct DetectProtocolLayer<D> {
    detect: D,
    peek_capacity: usize,
}

// === impl DetectProtoLayer ===
impl<D> DetectProtocolLayer<D> {
    const DEFAULT_CAPACITY: usize = 8192;

    pub fn new(detect: D) -> DetectProtocolLayer<D> {
        DetectProtocolLayer {
            detect,
            peek_capacity: Self::DEFAULT_CAPACITY,
        }
    }

    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.peek_capacity = capacity;
        self
    }
}

impl<D: Clone, S> tower::layer::Layer<S> for DetectProtocolLayer<D> {
    type Service = DetectProtocol<D, S>;

    fn layer(&self, inner: S) -> Self::Service {
        DetectProtocol {
            detect: self.detect.clone(),
            peek_capacity: self.peek_capacity,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DetectProtocol<D, S> {
    detect: D,
    peek_capacity: usize,
    inner: S,
}

pub enum DetectProtocolFuture<T, D, S>
where
    S: tower::Service<(D::Target, BoxedIo)>,
    D: Detect<T>,
{
    Detected(S::Future),
    Detecting {
        inner_svc: S,
        detect: D,
        inner: PeekAndDetect<T, D>,
    },
}

impl<T, D, S> tower::Service<(T, BoxedIo)> for DetectProtocol<D, S>
where
    D: Detect<T>,
    S: tower::Service<(D::Target, BoxedIo)>,
    S: Clone,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = DetectProtocolFuture<T, D, S>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, (target, io): (T, BoxedIo)) -> Self::Future {
        match self.detect.detect_before_peek(target) {
            Ok(detected) => {
                let inner_fut = self.inner.call((detected, io));
                return DetectProtocolFuture::Detected(inner_fut);
            }

            Err(target) => DetectProtocolFuture::Detecting {
                inner_svc: self.inner.clone(),
                detect: self.detect.clone(),
                inner: PeekAndDetect::Peek(
                    Some(target),
                    Peek::with_capacity(self.peek_capacity, io),
                ),
            },
        }
    }
}

impl<T, D, S> Future for DetectProtocolFuture<T, D, S>
where
    D: Detect<T>,
    S: tower::Service<(D::Target, BoxedIo)>,
    S::Error: Into<Error>,
{
    type Item = S::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self {
                DetectProtocolFuture::Detected(inner_fut) => {
                    return inner_fut.poll().map_err(Into::into)
                }
                DetectProtocolFuture::Detecting {
                    inner_svc,
                    ref detect,
                    ref mut inner,
                } => match inner {
                    PeekAndDetect::Detected(ref mut io) => {
                        try_ready!(inner_svc.poll_ready().map_err(Into::into));
                        let io = io.take().expect("polled after complete");
                        let inner_fut = inner_svc.call(io);
                        *self = DetectProtocolFuture::Detected(inner_fut);
                    }
                    PeekAndDetect::Peek(ref mut target, ref mut peek) => match peek.poll() {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(io)) => {
                            let target = detect.detect_peeked_prefix(
                                target.take().expect("polled after complete"),
                                io.prefix().as_ref(),
                            );
                            let inner_fut = inner_svc.call((target, BoxedIo::new(io)));
                            *self = DetectProtocolFuture::Detected(inner_fut);
                        }

                        Err(err) => return Err(Error::from(err)),
                    },
                },
            }
        }
    }
}
