#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

use futures::{prelude::*, select_biased};
use linkerd_error::Recover;
use linkerd_stack::{Service, ServiceExt};
use std::task::{Context, Poll};
use tokio::sync::watch;
use tracing::{debug, trace};

/// A service that streams updates from an inner service into a `tokio::sync::watch::Receiver` on a
/// background task.
///
/// The inner service's `poll_ready` is not expected to fail. If it does fail, though, these
/// failures must not be fatal. Clients may be reused after returning an error.
#[derive(Clone, Debug)]
pub struct StreamWatch<R, S> {
    recover: R,
    inner: S,
}

type Result<U> = std::result::Result<U, tonic::Status>;

type InnerStream<U> = futures::stream::BoxStream<'static, Result<U>>;

type InnerRsp<U> = tonic::Response<InnerStream<U>>;

type OuterRsp<U> = tonic::Response<watch::Receiver<U>>;

// === impl StreamWatch ===

impl<R, S> StreamWatch<R, S> {
    pub fn new(recover: R, inner: S) -> Self {
        Self { recover, inner }
    }
}

impl<R, S> StreamWatch<R, S>
where
    S: Clone + Send + 'static,
    R: Recover<tonic::Status> + Send + Clone + 'static,
    R::Backoff: Unpin + Send,
{
    pub async fn spawn_watch<T, U>(mut self, target: T) -> Result<OuterRsp<U>>
    where
        T: Clone + Send + Sync + 'static,
        U: Send + Sync + 'static,
        S: Service<T, Response = InnerRsp<U>, Error = tonic::Status>,
        S::Future: Send,
    {
        // Get an update and stream or return None.
        let (init, rsp) = self.init(&target, None).await?;

        // XXX Response::into_parts isn't public.
        let metadata = rsp.metadata().clone();

        // Spawn a background task to keep the profile watch up-to-date until all copies of `rx`
        // have dropped.
        let (tx, rx) = watch::channel(init);
        tokio::spawn(self.publish_updates(target, tx, rsp.into_inner()));

        let mut rsp = tonic::Response::new(rx);
        *rsp.metadata_mut() = metadata;
        Ok(rsp)
    }

    /// Initiates a lookup stream and obtains the first profile from it.
    ///
    /// If the call fails, recovery and back-off are applied.
    async fn init<T, U>(
        &mut self,
        target: &T,
        mut backoff: Option<R::Backoff>,
    ) -> Result<(U, InnerRsp<U>)>
    where
        T: Clone + Send + Sync + 'static,
        S: Service<T, Response = InnerRsp<U>, Error = tonic::Status>,
        S::Future: Send,
    {
        loop {
            tracing::trace!("Awaiting readiness");
            let status = match self.inner.ready().await {
                Ok(svc) => {
                    tracing::trace!("Issuing request");
                    match svc.call(target.clone()).await {
                        Ok(mut rsp) => match Self::next(rsp.get_mut()).await {
                            Ok(init) => return Ok((init, rsp)),
                            Err(status) => {
                                tracing::debug!(%status, "Stream failed");
                                status
                            }
                        },
                        Err(status) => {
                            tracing::debug!(%status, "Request failed");
                            status
                        }
                    }
                }
                Err(status) => {
                    tracing::debug!(%status, "Service did not become ready");
                    status
                }
            };

            let mut new_backoff = self.recover.recover(status)?;
            tracing::debug!("Recovering");
            if let Some(b) = backoff.as_mut() {
                // If there's a already a backoff, wait for it; but if the stream ends, then the
                // newly-obtained backoff is used.
                if b.next().await.is_none() {
                    tracing::trace!("Old backoff exhausted; using new backoff");
                    backoff = new_backoff.next().await.map(move |()| new_backoff);
                }
            } else {
                tracing::trace!("Using new backoff");
                backoff = new_backoff.next().await.map(move |()| new_backoff);
            }
            tracing::trace!("Backed off");
        }
    }

    // Publishes updates on `tx` from the stream, recovering and applying backoff backoff as
    // necessary.
    async fn publish_updates<T, U>(
        mut self,
        target: T,
        tx: watch::Sender<U>,
        mut stream: InnerStream<U>,
    ) where
        T: Clone + Send + Sync + 'static,
        U: Send + Sync + 'static,
        S: Service<T, Response = InnerRsp<U>, Error = tonic::Status>,
        S::Future: Send,
    {
        loop {
            select_biased! {
                // If all of the receivers are dropped, stop the task.
                _ = tx.closed().fuse() => {
                    trace!("Receivers dropped");
                    return;
                },

                // Otherwise, continue to get new profile versions and update the watch. The stream
                // may be re-instantiated each time
                res = self.recovering_next(&target, &mut stream).fuse() => match res {
                    Ok(profile) => {
                        let _ = tx.send(profile);
                    }
                    Err(status) => {
                        debug!(%status, "Profile stream failed");
                        return;
                    }
                },
            }
        }
    }

    /// Gets the next profile from the stream
    ///
    /// If the stream or lookup fails in a recoverable way, back-offs are applied and `stream` is
    /// updated to point at the updated stream..
    #[inline]
    async fn recovering_next<T, U>(&mut self, target: &T, stream: &mut InnerStream<U>) -> Result<U>
    where
        T: Clone + Send + Sync + 'static,
        S: Service<T, Response = InnerRsp<U>, Error = tonic::Status>,
        S::Future: Send,
    {
        match Self::next(stream).await {
            Ok(u) => Ok(u),
            Err(status) => {
                // Use the streaming error to get a backoff that can be applied if the next lookup
                // fails.
                let backoff = self.recover.recover(status)?;
                let (item, rsp) = self.init(target, Some(backoff)).await?;
                *stream = rsp.into_inner();
                Ok(item)
            }
        }
    }

    // Reads the next profile off of the given stream and, if it succeeds, returns the profile and
    // the stream.
    #[inline]
    async fn next<U>(stream: &mut InnerStream<U>) -> Result<U> {
        stream
            .try_next()
            .await?
            .ok_or_else(|| tonic::Status::ok("stream ended"))
    }
}

impl<T, U, R, S> Service<T> for StreamWatch<R, S>
where
    T: Clone + Send + Sync + 'static,
    U: Send + Sync + 'static,
    S: Service<T, Response = InnerRsp<U>, Error = tonic::Status> + Clone + Send + 'static,
    S::Future: Send,
    R: Recover<tonic::Status> + Send + Clone + 'static,
    R::Backoff: Unpin + Send,
{
    type Response = OuterRsp<U>;
    type Error = tonic::Status;
    type Future = futures::future::BoxFuture<'static, Result<OuterRsp<U>>>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<()>> {
        // The client is cloned into the response future and driven to readiness.
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, t: T) -> Self::Future {
        Box::pin(self.clone().spawn_watch(t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkerd_error::{recover, Error};
    use linkerd_stack::{layer::Layer, MapErrLayer};
    use tokio::{sync::mpsc, time};
    use tokio_stream::wrappers::{IntervalStream, ReceiverStream};
    use tower_test::mock;

    #[tokio::test(flavor = "current_thread")]
    async fn watch_reconnect() {
        let _trace = linkerd_tracing::test::trace_init();

        time::pause();

        let (mock, mut handle) = mock::pair::<(), InnerRsp<u16>>();
        let map_err = MapErrLayer::new(|e: Error| match e.downcast_ref::<tonic::Status>() {
            Some(status) => tonic::Status::new(status.code(), status.to_string()),
            None => tonic::Status::internal(e.to_string()),
        });
        let watch = StreamWatch::new(recover::Immediately::default(), map_err.layer(mock));

        handle.allow(1);
        let (tx0, rx0) = mpsc::channel::<Result<u16>>(3);
        let send_req = handle.next_request().map(move |req| {
            let ((), rsp) = req.unwrap();
            rsp.send_response(tonic::Response::new(Box::pin(ReceiverStream::new(rx0))))
        });
        let (_, _, rx) = tokio::join!(tx0.send(Ok(123u16)), send_req, watch.spawn_watch(()));
        let rx = rx.unwrap().into_inner();

        assert_eq!(*rx.borrow(), 123);

        tx0.send(Err(tonic::Status::ok("disconnect")))
            .await
            .unwrap();

        handle.allow(1);
        let (tx1, rx1) = mpsc::channel(3);
        let send_req = handle.next_request().map(move |req| {
            let ((), rsp) = req.unwrap();
            rsp.send_response(tonic::Response::new(Box::pin(ReceiverStream::new(rx1))))
        });
        let (_, _) = tokio::join!(tx1.send(Ok(345u16)), send_req);

        // We need to give the background task an opportunity to process the update so this yields
        // control. The actual sleep duration is unimportant (and time is mocked, anyway).
        time::sleep(time::Duration::from_secs(1)).await;

        assert_eq!(*rx.borrow(), 345);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn block_on_initial_failure() {
        let _trace = linkerd_tracing::test::trace_init();

        time::pause();

        let (mock, mut handle) = mock::pair::<(), InnerRsp<u16>>();
        let map_err = MapErrLayer::new(|e: Error| match e.downcast_ref::<tonic::Status>() {
            Some(status) => tonic::Status::new(status.code(), status.to_string()),
            None => tonic::Status::internal(e.to_string()),
        });
        let recover = |_: tonic::Status| {
            Ok(IntervalStream::new(time::interval_at(
                time::Instant::now() + time::Duration::from_secs(2),
                time::Duration::from_secs(2),
            ))
            .map(|_| {
                tracing::debug!("backoff fired");
            }))
        };
        let watch = StreamWatch::new(recover, map_err.layer(mock));

        handle.allow(1);
        let send_err = handle.next_request().map(move |req| {
            let ((), rsp) = req.unwrap();
            rsp.send_error(tonic::Status::internal("spooky error"))
        });

        // Start waiting for a watch.
        tokio::pin! {
           let waiting = watch.spawn_watch(())
                .map(|t| t.expect("watch lookup must not fail").into_inner());
        }

        // We should send the error, but it should begin recovery and we should continue waiting
        // for a watch.
        tokio::select! {
            res = time::timeout(time::Duration::from_secs(1), send_err) => res.expect("error must have been sent"),
            _ = &mut waiting => panic!("Watch can't be obtained yet"),
        }

        handle.allow(1);
        let (tx0, rx0) = mpsc::channel::<Result<u16>>(3);
        tx0.send(Ok(123u16)).await.expect("receiver must be held");
        let send = tokio::spawn(async move {
            let ((), rsp) = handle.next_request().await.unwrap();
            rsp.send_response(tonic::Response::new(Box::pin(ReceiverStream::new(rx0))))
        });

        tokio::select! {
            _ = time::sleep(time::Duration::from_secs(1)) => {}
            _ = send => panic!("response shouldn't be sent until the timeout elapses"),
        }

        time::sleep(time::Duration::from_secs(1)).await;

        let watch = waiting.await;
        assert_eq!(*watch.borrow(), 123);
    }
}
