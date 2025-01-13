use crate::upgrade::Http11Upgrade;
use futures::ready;
use http_body::Body;
use linkerd_error::Result;
use linkerd_http_box::BoxBody;
use pin_project::{pin_project, pinned_drop};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tracing::debug;

/// Provides optional HTTP/1.1 upgrade support on the body.
#[pin_project(PinnedDrop)]
#[derive(Debug)]
pub struct UpgradeBody<B = BoxBody> {
    /// The inner [`Body`] being wrapped.
    #[pin]
    body: B,
    /// A potential HTTP/1.1 upgrade.
    ///
    /// If `Some(_)`, contains a channel and an upgrade future, representing transport i/o
    /// that will service the ongoing connection. When this body is dropped, the upgrade future
    /// will be sent to the upgrade channel.
    upgrade: Option<(Http11Upgrade, hyper::upgrade::OnUpgrade)>,
}

// === impl UpgradeBody ===

impl<B> UpgradeBody<B> {
    pub(crate) fn new(
        body: B,
        upgrade: Option<(Http11Upgrade, hyper::upgrade::OnUpgrade)>,
    ) -> Self {
        Self { body, upgrade }
    }
}

impl<B> Body for UpgradeBody<B>
where
    B: Body,
    B::Error: std::fmt::Display,
{
    type Data = B::Data;
    type Error = B::Error;

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        // Poll the next chunk from the body.
        let this = self.project();
        let body = this.body;
        let data = ready!(body.poll_data(cx));

        // Log errors.
        if let Some(Err(e)) = &data {
            debug!("http body error: {}", e);
        }

        Poll::Ready(data)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        // Poll the trailers from the body.
        let this = self.project();
        let body = this.body;
        let trailers = ready!(body.poll_trailers(cx));

        // Log errors.
        if let Err(e) = &trailers {
            debug!("http trailers error: {}", e);
        }

        Poll::Ready(trailers)
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

impl<B: Default> Default for UpgradeBody<B> {
    fn default() -> Self {
        Self {
            body: B::default(),
            upgrade: None,
        }
    }
}

#[pinned_drop]
impl<B> PinnedDrop for UpgradeBody<B> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        // If an HTTP/1 upgrade was wanted, send the upgrade future.
        if let Some((upgrade, on_upgrade)) = this.upgrade.take() {
            upgrade.insert_half(on_upgrade);
        }
    }
}
