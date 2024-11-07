use crate::keys::ServiceKeys;
use linkerd_stack::{NewService, Service};
use std::task::{Context, Poll};

#[derive(Debug)]
pub(crate) struct FirstAvailableSelection<S> {
    backends: Vec<S>,

    /// Stores the index of the backend that has been polled to ready. The
    /// service at this index will be used on the next invocation of
    /// `Service::call`.
    ready_idx: Option<usize>,
}

impl<S> FirstAvailableSelection<S> {
    pub fn new<K, N>(keys: &ServiceKeys<K>, make_svc: N) -> Self
    where
        N: for<'a> NewService<&'a K, Service = S>,
    {
        Self {
            backends: keys
                .iter()
                .map(|&id| make_svc.new_service(keys.get(id)))
                .collect(),
            ready_idx: None,
        }
    }

    #[cfg(test)]
    pub fn get_ready_idx(&self) -> Option<usize> {
        self.ready_idx
    }
}

impl<S: Clone> Clone for FirstAvailableSelection<S> {
    fn clone(&self) -> Self {
        Self {
            backends: self.backends.clone(),
            // Clear the ready index so that the new clone must become ready
            // independently.
            ready_idx: None,
        }
    }
}

impl<Req, S> Service<Req> for FirstAvailableSelection<S>
where
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // If we've already chosen a ready index, then skip polling.
        if self.ready_idx.is_some() {
            return Poll::Ready(Ok(()));
        }

        for (idx, svc) in self.backends.iter_mut().enumerate() {
            if svc.poll_ready(cx)?.is_ready() {
                self.ready_idx = Some(idx);
                return Poll::Ready(Ok(()));
            }
        }
        debug_assert!(self.ready_idx.is_none());
        Poll::Pending
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let idx = self
            .ready_idx
            .take()
            .expect("poll_ready must be called first");

        let svc = self.backends.get_mut(idx).expect("index must exist");

        svc.call(req)
    }
}
