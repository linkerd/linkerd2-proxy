use crate::{layer, NewService};
use std::{fmt, sync::Arc};

pub struct BoxNewService<T, S> {
    inner: Arc<dyn NewService<T, Service = S> + Send + Sync>,
}

impl<T, S> BoxNewService<T, S> {
    pub fn layer<N>() -> impl layer::Layer<N, Service = Self> + Clone + Copy
    where
        N: NewService<T, Service = S> + Clone + Send + Sync + 'static,
        S: Send + 'static,
    {
        layer::mk(Self::new)
    }

    pub fn new<N>(inner: N) -> Self
    where
        N: NewService<T, Service = S> + Send + Sync + 'static,
        S: Send + 'static,
    {
        Self {
            inner: Arc::new(inner),
        }
    }
}

impl<T, S> Clone for BoxNewService<T, S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T, S> NewService<T> for BoxNewService<T, S> {
    type Service = S;

    fn new_service(&self, t: T) -> S {
        self.inner.new_service(t)
    }
}

impl<T, S> fmt::Debug for BoxNewService<T, S> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("BoxNewService").finish()
    }
}
