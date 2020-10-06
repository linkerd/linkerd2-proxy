use futures::future::Either;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::{delay_for, Delay, Instant};

/// A service which falls back to a secondary service if the primary service
/// takes too long to become ready.
#[derive(Debug)]
pub struct SwitchReady<A, B> {
    primary: A,
    secondary: B,
    switch_after: Duration,
    delay: Delay,
    state: State,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum State {
    Primary,
    Waiting,
    Secondary,
}

// === impl SwitchReady ===

impl<A, B> SwitchReady<A, B> {
    /// Returns a new `SwitchReadyLayer`.
    ///
    /// This will forward requests to the primary service, unless it takes over
    /// `switch_after` duration to become ready. If the duration is exceeded,
    /// the `secondary` service is used until the primary service becomes ready again.
    pub fn new(primary: A, secondary: B, switch_after: Duration) -> Self {
        Self {
            primary,
            secondary,
            switch_after,
            // the delay is reset whenever the service becomes unready; this
            // initial one will never actually be used, so it's okay to start it
            // now.
            delay: delay_for(switch_after),
            state: State::Primary,
        }
    }
}

impl<A, B, R> tower::Service<R> for SwitchReady<A, B>
where
    A: tower::Service<R>,
    B: tower::Service<R, Response = A::Response, Error = A::Error>,
{
    type Future = Either<A::Future, B::Future>;
    type Response = A::Response;
    type Error = A::Error;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            tracing::trace!(?self.state, "SwitchReady::poll");
            match self.state {
                State::Primary => match self.primary.poll_ready(cx) {
                    Poll::Ready(x) => return Poll::Ready(x),
                    Poll::Pending => {
                        tracing::trace!("primary service pending");
                        self.delay.reset(Instant::now() + self.switch_after);
                        self.state = State::Waiting;
                    }
                },
                State::Waiting => {
                    match self.primary.poll_ready(cx) {
                        Poll::Ready(Ok(())) => {
                            tracing::trace!("primary service became ready");
                            self.state = State::Primary;
                            return Poll::Ready(Ok(()));
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Pending => {}
                    };
                    if let Poll::Pending = Pin::new(&mut self.delay).poll(cx) {
                        return Poll::Pending;
                    }
                    tracing::trace!("delay expired, switching to secondary");
                    self.state = State::Secondary;
                }
                State::Secondary => {
                    return if let Poll::Ready(x) = self.primary.poll_ready(cx) {
                        tracing::trace!("primary service became ready, switching back");
                        self.state = State::Primary;
                        Poll::Ready(x)
                    } else {
                        self.secondary.poll_ready(cx)
                    };
                }
            };
        }
    }

    fn call(&mut self, req: R) -> Self::Future {
        tracing::trace!(?self.state, "SwitchReady::call");
        match self.state {
            State::Primary => Either::Left(self.primary.call(req)),
            State::Secondary => Either::Right(self.secondary.call(req)),
            State::Waiting => panic!("called before ready!"),
        }
    }
}

impl<A: Clone, B: Clone> Clone for SwitchReady<A, B> {
    fn clone(&self) -> Self {
        Self {
            primary: self.primary.clone(),
            secondary: self.secondary.clone(),
            switch_after: self.switch_after,
            // Reset the state and delay; each clone of the underlying services
            // may become ready independently (e.g. semaphore).
            delay: delay_for(self.switch_after),
            state: State::Primary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test::{assert_pending, assert_ready_err, assert_ready_ok};
    use tower_test::mock;

    #[tokio::test]
    async fn primary_first() {
        let _ = tracing_subscriber::fmt::try_init();

        let dur = Duration::from_millis(100);
        let (b, mut b_handle) = mock::pair::<(), ()>();

        let (mut switch, mut a_handle) =
            mock::spawn_with(move |a| SwitchReady::new(a, b.clone(), dur));
        b_handle.allow(0);
        a_handle.allow(1);

        assert_ready_ok!(switch.poll_ready());

        let call = switch.call(());
        let (_, rsp) = a_handle.next_request().await.expect("service not called");

        rsp.send_response(());
        call.await.expect("call succeeds");
    }

    #[tokio::test]
    async fn primary_becomes_ready() {
        let _ = tracing_subscriber::fmt::try_init();

        let dur = Duration::from_millis(100);
        let (b, mut b_handle) = mock::pair::<(), ()>();
        b_handle.allow(0);

        let (mut switch, mut a_handle) =
            mock::spawn_with(move |a| SwitchReady::new(a, b.clone(), dur));

        // Initially, nothing happens.
        a_handle.allow(0);
        assert_pending!(switch.poll_ready());

        // The primary service becomes ready.
        a_handle.allow(1);
        assert_ready_ok!(switch.poll_ready());

        let call = switch.call(());
        let (_, rsp) = a_handle.next_request().await.expect("service not called");

        rsp.send_response(());
        call.await.expect("call succeeds");
    }

    #[tokio::test]
    async fn primary_times_out() {
        let _ = tracing_subscriber::fmt::try_init();

        let dur = Duration::from_millis(100);
        let (b, mut b_handle) = mock::pair::<(), ()>();
        b_handle.allow(0);

        let (mut switch, mut a_handle) =
            mock::spawn_with(move |a| SwitchReady::new(a, b.clone(), dur));

        // Initially, nothing happens.
        a_handle.allow(0);
        assert_pending!(switch.poll_ready());

        // Idle out the primary service.
        delay_for(dur + Duration::from_millis(1)).await;
        assert_pending!(switch.poll_ready());

        // The secondary service becomes ready.
        b_handle.allow(1);
        assert_ready_ok!(switch.poll_ready());

        let call = switch.call(());
        let (_, rsp) = b_handle.next_request().await.expect("service not called");

        rsp.send_response(());
        call.await.expect("call succeeds");
    }

    #[tokio::test]
    async fn primary_times_out_and_becomes_ready() {
        let _ = tracing_subscriber::fmt::try_init();

        let dur = Duration::from_millis(100);
        let (b, mut b_handle) = mock::pair::<(), ()>();
        b_handle.allow(0);

        let (mut switch, mut a_handle) =
            mock::spawn_with(move |a| SwitchReady::new(a, b.clone(), dur));

        // Initially, nothing happens.
        a_handle.allow(0);
        assert_pending!(switch.poll_ready());

        delay_for(dur + Duration::from_millis(1)).await;
        assert_pending!(switch.poll_ready());

        // The secondary service becomes ready.
        b_handle.allow(1);
        assert_ready_ok!(switch.poll_ready());

        let call = switch.call(());
        let (_, rsp) = b_handle.next_request().await.expect("service not called");

        rsp.send_response(());
        call.await.expect("call succeeds");

        // The primary service becomes ready.
        a_handle.allow(1);
        assert_ready_ok!(switch.poll_ready());

        let call = switch.call(());
        let (_, rsp) = a_handle.next_request().await.expect("service not called");

        rsp.send_response(());
        call.await.expect("call succeeds");

        // delay for _half_ the duration. *not* long enough to time out.
        assert_pending!(switch.poll_ready());
        delay_for(dur / 2).await;
        assert_pending!(switch.poll_ready());

        // The primary service becomes ready again.
        a_handle.allow(1);
        assert_ready_ok!(switch.poll_ready());

        let call = switch.call(());
        let (_, rsp) = a_handle.next_request().await.expect("service not called");

        rsp.send_response(());
        call.await.expect("call succeeds");
    }

    #[tokio::test]
    async fn delays_dont_add_up() {
        let _ = tracing_subscriber::fmt::try_init();

        let dur = Duration::from_millis(100);
        let (b, mut b_handle) = mock::pair::<(), ()>();
        b_handle.allow(0);

        let (mut switch, mut a_handle) =
            mock::spawn_with(move |a| SwitchReady::new(a, b.clone(), dur));

        // Initially, nothing happens.
        a_handle.allow(0);
        assert_pending!(switch.poll_ready());

        // delay for _half_ the duration. *not* long enough to time out.
        delay_for(dur / 2).await;
        assert_pending!(switch.poll_ready());

        // The primary service becomes ready.
        a_handle.allow(1);
        assert_ready_ok!(switch.poll_ready());

        let call = switch.call(());
        let (_, rsp) = a_handle.next_request().await.expect("service not called");

        rsp.send_response(());
        call.await.expect("call succeeds");

        // delay for half the duration again
        assert_pending!(switch.poll_ready());
        delay_for(dur / 2).await;
        assert_pending!(switch.poll_ready());

        // The primary service becomes ready.
        a_handle.allow(1);
        assert_ready_ok!(switch.poll_ready());

        let call = switch.call(());
        let (_, rsp) = a_handle.next_request().await.expect("service not called");

        rsp.send_response(());
        call.await.expect("call succeeds");

        // delay for half the duration a third time. even though we've delayed
        // for longer than the total duration after which we idle out the
        // primary service, this should be reset every time the primary becomes ready.
        assert_pending!(switch.poll_ready());
        delay_for(dur / 2).await;
        assert_pending!(switch.poll_ready());

        // The primary service becomes ready.
        a_handle.allow(1);
        assert_ready_ok!(switch.poll_ready());

        let call = switch.call(());
        let (_, rsp) = a_handle.next_request().await.expect("service not called");

        rsp.send_response(());
        call.await.expect("call succeeds");
    }

    #[tokio::test]
    async fn propagates_errors() {
        let _ = tracing_subscriber::fmt::try_init();

        let dur = Duration::from_millis(100);
        let (b, mut b_handle) = mock::pair::<(), ()>();
        b_handle.allow(0);

        let (mut switch, mut a_handle) =
            mock::spawn_with(move |a| SwitchReady::new(a, b.clone(), dur));

        // Initially, nothing happens.
        a_handle.allow(0);
        assert_pending!(switch.poll_ready());

        // Error the primary
        a_handle.send_error("lol");
        assert_ready_err!(switch.poll_ready());

        delay_for(dur + Duration::from_millis(1)).await;
        assert_pending!(switch.poll_ready());

        b_handle.send_error("lol");
        assert_ready_err!(switch.poll_ready());
    }
}
