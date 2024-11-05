use super::TcpMetricsParams;
use linkerd_errno::Errno;
use linkerd_error::{self as errors, Error};
use linkerd_io as io;
use linkerd_metrics::prom::EncodeLabelSetMut;
use linkerd_stack::{layer, NewService, Param, Service, ServiceExt};
use std::hash::Hash;
use std::{
    fmt::Debug,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub struct NewInstrumentConnection<N, L: Clone> {
    inner: N,
    params: TcpMetricsParams<L>,
}

#[derive(Clone, Debug)]
pub struct InstrumentConnection<T, N, L: Clone> {
    target: T,
    inner: N,
    params: TcpMetricsParams<L>,
}

// === impl NewInstrumentConnection ===

impl<N, L: Clone> NewInstrumentConnection<N, L> {
    pub fn layer(params: TcpMetricsParams<L>) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            params: params.clone(),
        })
    }
}

impl<T, N, L> NewService<T> for NewInstrumentConnection<N, L>
where
    N: Clone,
    L: Clone,
{
    type Service = InstrumentConnection<T, N, L>;

    fn new_service(&self, target: T) -> Self::Service {
        InstrumentConnection::new(target, self.inner.clone(), self.params.clone())
    }
}

// === impl InstrumentConnection ===

impl<T, N, L: Clone> InstrumentConnection<T, N, L> {
    fn new(target: T, inner: N, params: TcpMetricsParams<L>) -> Self {
        Self {
            target,
            inner,
            params,
        }
    }
}

impl<T, I, N, S, L> Service<I> for InstrumentConnection<T, N, L>
where
    L: Clone + Hash + Eq + EncodeLabelSetMut + Debug + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
    T: Param<L>,
    I: io::AsyncRead + io::AsyncWrite + Debug + Send + Unpin + 'static,
    N: NewService<T, Service = S> + Clone + Send + 'static,
    S: Service<I> + Send,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        const ERRNO_UNKNOWN: i32 = 0;
        let target = self.target.clone();
        let new_accept = self.inner.clone();
        let labels: L = target.param();
        let params = self.params.clone();

        Box::pin(async move {
            let metrics_open = params.metrics_open(labels.clone());
            let svc = new_accept.new_service(target);
            metrics_open.inc();

            match svc.oneshot(io).await.map_err(Into::into) {
                Ok(result) => {
                    params.metrics_closed(labels, None).inc();
                    Ok(result)
                }
                Err(error) => {
                    let errno = Some(match errors::cause_ref::<Errno>(&*error) {
                        Some(errno) => *errno,
                        None => ERRNO_UNKNOWN.into(),
                    });

                    params.metrics_closed(labels, errno).inc();
                    Err(error)
                }
            }
        })
    }
}
