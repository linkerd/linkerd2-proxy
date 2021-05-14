use super::NewService;
use linkerd_error::Error;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;
use tower::util::Either;
use tracing::{debug, trace};

/// A service which falls back to a secondary service if the primary service
/// takes too long to become ready.
#[derive(Debug)]
pub struct SwitchReady<A, B> {
    primary: A,
    secondary: B,
    switch_after: time::Duration,
    sleep: Pin<Box<time::Sleep>>,
    state: State,
}

#[derive(Debug, Clone)]
pub struct NewSwitchReady<A, B> {
    new_primary: A,
    new_secondary: B,
    switch_after: time::Duration,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum State {
    Primary,
    Waiting,
    Secondary,
}

// === impl NewSwitchReady ===

impl<A, B> NewSwitchReady<A, B> {
    /// Returns a new `NewSwitchReady`.
    ///
    /// This will forward requests to the primary service, unless it takes over
    /// `switch_after` duration to become ready. If the duration is exceeded,
    /// the `secondary` service is used until the primary service becomes ready again.
    pub fn new(new_primary: A, new_secondary: B, switch_after: time::Duration) -> Self {
        Self {
            new_primary,
            new_secondary,
            switch_after,
        }
    }
}

impl<A, B, T> NewService<T> for NewSwitchReady<A, B>
where
    T: Clone,
    A: NewService<T>,
    B: NewService<T>,
{
    type Service = SwitchReady<A::Service, B::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        SwitchReady::new(
            self.new_primary.new_service(target.clone()),
            self.new_secondary.new_service(target),
            self.switch_after,
        )
    }
}

// === impl SwitchReady ===

impl<A, B> SwitchReady<A, B> {
    /// Returns a new `SwitchReady`.
    ///
    /// This will forward requests to the primary service, unless it takes over
    /// `switch_after` duration to become ready. If the duration is exceeded,
    /// the `secondary` service is used until the primary service becomes ready again.
    pub fn new(primary: A, secondary: B, switch_after: time::Duration) -> Self {
        Self {
            primary,
            secondary,
            switch_after,
            // The sleep is reset whenever the service becomes unready; this
            // initial one will never actually be used, so it's okay to start it
            // now.
            sleep: Box::pin(time::sleep(time::Duration::default())),
            state: State::Primary,
        }
    }
}

impl<A, B, R> tower::Service<R> for SwitchReady<A, B>
where
    A: tower::Service<R>,
    A::Error: Into<Error>,
    B: tower::Service<R, Response = A::Response>,
    B::Error: Into<Error>,
{
    type Response = A::Response;
    type Error = Error;
    type Future = Either<A::Future, B::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            trace!(state = ?self.state, "SwitchReady::poll");
            match self.state {
                // When in the primary state, poll the primary service and if
                // it's not ready, start a timer and transition into the waiting
                // state.
                State::Primary => match self.primary.poll_ready(cx).map_err(Into::into) {
                    Poll::Ready(ready) => return Poll::Ready(ready),
                    Poll::Pending => {
                        trace!(delay = ?self.switch_after, "Primary service pending");
                        self.sleep
                            .as_mut()
                            .reset(time::Instant::now() + self.switch_after);
                        self.state = State::Waiting;
                    }
                },

                // While waiting, check the timer. If the timer has expired, go
                // into the secondary state. Otherwise, poll the primary service
                // to see if it's recovered.
                State::Waiting => match self.sleep.as_mut().poll(cx) {
                    Poll::Ready(()) => {
                        debug!(after = ?self.switch_after, "Switching to secondary service");
                        self.state = State::Secondary;
                    }
                    Poll::Pending => match self.primary.poll_ready(cx).map_err(Into::into) {
                        Poll::Ready(ready) => {
                            trace!(?ready, "Primary service became ready");
                            self.state = State::Primary;
                            return Poll::Ready(ready);
                        }
                        Poll::Pending => return Poll::Pending,
                    },
                },

                // Always poll the primary service first so it has a chance to
                // become ready. If it's ready, change the state to primary and
                // return the readiness value.
                State::Secondary => match self.primary.poll_ready(cx).map_err(Into::into) {
                    Poll::Ready(ready) => {
                        debug!(?ready, "Reverting to primary service");
                        self.state = State::Primary;
                        return Poll::Ready(ready);
                    }
                    Poll::Pending => {
                        // The primary service is still pending so, poll the
                        // secondary service.
                        return self.secondary.poll_ready(cx).map_err(Into::into);
                    }
                },
            };
        }
    }

    fn call(&mut self, req: R) -> Self::Future {
        trace!(state = ?self.state, "SwitchReady::call");
        match self.state {
            State::Primary => Either::A(self.primary.call(req)),
            State::Secondary => Either::B(self.secondary.call(req)),
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
            // Reset the state and the sleep; each clone of the underlying services
            // may become ready independently (e.g. semaphore).
            sleep: Box::pin(time::sleep(time::Duration::default())),
            state: State::Primary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test::{assert_pending, assert_ready_err, assert_ready_ok};
    use tower_test::mock;

    #[tokio::test(flavor = "current_thread")]
    async fn primary_first() {
        let _trace = linkerd_tracing::test::trace_init();

        time::pause();
        let dur = time::Duration::from_millis(100);
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

    #[tokio::test(flavor = "current_thread")]
    async fn primary_becomes_ready() {
        let _trace = linkerd_tracing::test::trace_init();

        time::pause();
        let dur = time::Duration::from_millis(100);
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

    #[tokio::test(flavor = "current_thread")]
    async fn primary_times_out() {
        let _trace = linkerd_tracing::test::trace_init();

        time::pause();
        let dur = time::Duration::from_millis(100);
        let (b, mut b_handle) = mock::pair::<(), ()>();
        b_handle.allow(0);

        let (mut switch, mut a_handle) =
            mock::spawn_with(move |a| SwitchReady::new(a, b.clone(), dur));

        // Initially, nothing happens.
        a_handle.allow(0);
        assert_pending!(switch.poll_ready());

        // Idle out the primary service.
        time::sleep(dur + time::Duration::from_millis(1)).await;
        assert_pending!(switch.poll_ready());

        // The secondary service becomes ready.
        b_handle.allow(1);
        assert_ready_ok!(switch.poll_ready());

        let call = switch.call(());
        let (_, rsp) = b_handle.next_request().await.expect("service not called");

        rsp.send_response(());
        call.await.expect("call succeeds");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn primary_times_out_and_becomes_ready() {
        let _trace = linkerd_tracing::test::trace_init();

        time::pause();
        let dur = time::Duration::from_millis(100);
        let (b, mut b_handle) = mock::pair::<(), ()>();
        b_handle.allow(0);

        let (mut switch, mut a_handle) =
            mock::spawn_with(move |a| SwitchReady::new(a, b.clone(), dur));

        // Initially, nothing happens.
        a_handle.allow(0);
        assert_pending!(switch.poll_ready());

        time::sleep(dur + time::Duration::from_millis(1)).await;
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
        time::sleep(dur / 2).await;
        assert_pending!(switch.poll_ready());

        // The primary service becomes ready again.
        a_handle.allow(1);
        assert_ready_ok!(switch.poll_ready());

        let call = switch.call(());
        let (_, rsp) = a_handle.next_request().await.expect("service not called");

        rsp.send_response(());
        call.await.expect("call succeeds");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delays_dont_add_up() {
        let _trace = linkerd_tracing::test::trace_init();

        time::pause();
        let dur = time::Duration::from_millis(100);
        let (b, mut b_handle) = mock::pair::<(), ()>();
        b_handle.allow(0);

        let (mut switch, mut a_handle) =
            mock::spawn_with(move |a| SwitchReady::new(a, b.clone(), dur));

        // Initially, nothing happens.
        a_handle.allow(0);
        assert_pending!(switch.poll_ready());

        // delay for _half_ the duration. *not* long enough to time out.
        time::sleep(dur / 2).await;
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
        time::sleep(dur / 2).await;
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
        time::sleep(dur / 2).await;
        assert_pending!(switch.poll_ready());

        // The primary service becomes ready.
        a_handle.allow(1);
        assert_ready_ok!(switch.poll_ready());

        let call = switch.call(());
        let (_, rsp) = a_handle.next_request().await.expect("service not called");

        rsp.send_response(());
        call.await.expect("call succeeds");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn propagates_primary_errors() {
        let _trace = linkerd_tracing::test::trace_init();

        time::pause();
        let dur = time::Duration::from_millis(100);
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
    }

    #[tokio::test(flavor = "current_thread")]
    async fn propagates_secondary_errors() {
        let _trace = linkerd_tracing::test::trace_init();

        time::pause();
        let dur = time::Duration::from_millis(100);
        let (b, mut b_handle) = mock::pair::<(), ()>();
        b_handle.allow(0);

        let (mut switch, mut a_handle) =
            mock::spawn_with(move |a| SwitchReady::new(a, b.clone(), dur));

        a_handle.allow(0);
        b_handle.send_error("lol");

        assert_pending!(switch.poll_ready());
        time::sleep(dur).await;
        assert_ready_err!(switch.poll_ready());
    }
}
