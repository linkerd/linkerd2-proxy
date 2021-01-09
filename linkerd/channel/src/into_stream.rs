use futures::stream::Stream;
use std::{
    pin::Pin,
    task::{Context, Poll},
};

use tokio::{
    sync::mpsc,
    time::{Instant, Interval},
};

pub trait IntoStream {
    fn into_stream(self) -> Streaming<Self>
    where
        Self: Sized,
    {
        Streaming(self)
    }
}

impl<T> IntoStream for T where Streaming<T>: Stream {}

#[derive(Clone, Debug)]
pub struct Streaming<T>(T);

impl<T> Stream for Streaming<mpsc::Receiver<T>> {
    type Item = T;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.as_mut().0.poll_recv(cx)
    }
}

impl<T> Stream for Streaming<mpsc::UnboundedReceiver<T>> {
    type Item = T;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.as_mut().0.poll_recv(cx)
    }
}

impl Stream for Streaming<Interval> {
    type Item = Instant;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.as_mut().0.poll_tick(cx).map(Some)
    }
}
