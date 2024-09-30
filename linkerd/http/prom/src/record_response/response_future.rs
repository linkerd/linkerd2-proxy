use super::{DurationFamily, StreamLabel};
use http_body::Body;
use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_metrics::prom::Counter;
use prometheus_client::metrics::family::Family;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{sync::oneshot, time};

#[pin_project::pin_project]
pub struct ResponseFuture<L, F>
where
    L: StreamLabel,
{
    #[pin]
    pub(crate) inner: F,
    pub(crate) state: Option<ResponseState<L>>,
}

/// Notifies the response labeler when the response body is flushed.
#[pin_project::pin_project(PinnedDrop)]
struct ResponseBody<L: StreamLabel> {
    #[pin]
    inner: BoxBody,
    state: Option<ResponseState<L>>,
}

/// Inner state used by [`ResponseFuture`] and [`ResponseBody`].
///
/// This is used to update Prometheus metrics across the response's lifecycle.
///
/// This is generic across an `L`-typed [`StreamLabel`], which bears responsibility for labelling
/// responses according to their status code and duration.
//
//  TODO(kate): this should not need to be public.
pub struct ResponseState<L: StreamLabel> {
    pub(super) labeler: L,
    /// The family of [`Counter`]s tracking response status code.
    pub(super) statuses: Family<L::StatusLabels, Counter>,
    /// The family of [`Histogram`]s tracking response durations.
    pub(super) duration: DurationFamily<L::DurationLabels>,
    /// Receives a timestamp noting when the service received a request.
    pub(super) start: oneshot::Receiver<time::Instant>,
}

// === impl ResponseFuture ===

impl<L, F> Future for ResponseFuture<L, F>
where
    L: StreamLabel,
    F: Future<Output = Result<http::Response<BoxBody>, Error>>,
{
    /// A [`ResponseFuture`] produces the same response as its inner `F`-typed future.
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // Poll the inner future, returning if it isn't ready yet.
        let res = futures::ready!(this.inner.poll(cx)).map_err(Into::into);

        // We got a response back! Take our state, and examine the output.
        let mut state = this.state.take();
        match res {
            Ok(rsp) => {
                let (head, inner) = rsp.into_parts();
                if let Some(ResponseState { labeler, .. }) = state.as_mut() {
                    labeler.init_response(&head);
                }

                // Call `end_stream` if the body is empty.
                if inner.is_end_stream() {
                    end_stream(&mut state, Ok(None));
                }
                Poll::Ready(Ok(http::Response::from_parts(
                    head,
                    BoxBody::new(ResponseBody { inner, state }),
                )))
            }
            Err(error) => {
                end_stream(&mut state, Err(&error));
                Poll::Ready(Err(error))
            }
        }
    }
}

// === impl ResponseBody ===

impl<L> http_body::Body for ResponseBody<L>
where
    L: StreamLabel,
{
    type Data = <BoxBody as http_body::Body>::Data;
    type Error = Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Error>>> {
        let mut this = self.project();
        let res =
            futures::ready!(this.inner.as_mut().poll_data(cx)).map(|res| res.map_err(Into::into));
        if let Some(Err(error)) = res.as_ref() {
            end_stream(this.state, Err(error));
        } else if (*this.inner).is_end_stream() {
            end_stream(this.state, Ok(None));
        }
        Poll::Ready(res)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Error>> {
        let this = self.project();
        let res = futures::ready!(this.inner.poll_trailers(cx)).map_err(Into::into);
        end_stream(this.state, res.as_ref().map(Option::as_ref));
        Poll::Ready(res)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

#[pin_project::pinned_drop]
impl<L> PinnedDrop for ResponseBody<L>
where
    L: StreamLabel,
{
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if this.state.is_some() {
            end_stream(this.state, Err(&super::RequestCancelled(()).into()));
        }
    }
}

fn end_stream<L>(
    state: &mut Option<ResponseState<L>>,
    res: Result<Option<&http::HeaderMap>, &Error>,
) where
    L: StreamLabel,
{
    let Some(ResponseState {
        duration,
        statuses: total,
        mut start,
        mut labeler,
    }) = state.take()
    else {
        return;
    };

    labeler.end_response(res);

    total.get_or_create(&labeler.status_labels()).inc();

    let elapsed = if let Ok(start) = start.try_recv() {
        time::Instant::now().saturating_duration_since(start)
    } else {
        time::Duration::ZERO
    };
    duration
        .get_or_create(&labeler.duration_labels())
        .observe(elapsed.as_secs_f64());
}
