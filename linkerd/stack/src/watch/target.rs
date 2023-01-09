use crate::NewService;
use std::{fmt, future::Future, marker::PhantomData};
use tokio::sync::watch;

pub struct NewWatchTarget<W, U, N> {
    inner: N,
    start_watch: U,
    _target: PhantomData<fn(W)>,
}

impl<W, U, N> NewWatchTarget<W, U, N>
where
    U: Clone,
{
    pub fn layer(
        start_watch: U,
    ) -> impl crate::layer::Layer<N, Service = NewWatchTarget<W, U, N>> + Clone {
        crate::layer::mk(move |inner| Self::new(start_watch.clone(), inner))
    }

    fn new(start_watch: U, inner: N) -> Self {
        Self {
            inner,
            start_watch,
            _target: PhantomData,
        }
    }
}

impl<W, U: Clone, N: Clone> Clone for NewWatchTarget<W, U, N> {
    fn clone(&self) -> Self {
        Self::new(self.start_watch.clone(), self.inner.clone())
    }
}

impl<W, U, N: fmt::Debug> fmt::Debug for NewWatchTarget<W, U, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewWatchTarget")
            .field("inner", &self.inner)
            .field("start_watch", &format_args!("..."))
            .finish()
    }
}

impl<T, W, U, F, N> NewService<T> for NewWatchTarget<W, U, N>
where
    U: Fn(T, watch::Sender<W>) -> F + Clone,
    F: Future + Send + 'static,
    F::Output: Send + 'static,
    N: NewService<(T, watch::Receiver<W>)>,
    T: Clone,
    W: Default, // XXX(eliza): ew
{
    type Service = N::Service;

    fn new_service(&self, target: T) -> Self::Service {
        let (tx, rx) = watch::channel(W::default());
        tokio::spawn((self.start_watch)(target.clone(), tx));
        self.inner.new_service((target, rx))
    }
}
