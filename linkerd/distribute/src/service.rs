use super::Distribution;
use crate::keys::{KeyId, ServiceKeys, WeightedServiceKeys};
use linkerd_stack::{NewService, Service};
use rand::{distributions::WeightedError, rngs::SmallRng, SeedableRng};
use std::{
    collections::HashMap,
    hash::Hash,
    sync::Arc,
    task::{Context, Poll},
};

/// A service that distributes requests over a set of backends.
#[derive(Debug)]
pub struct Distribute<K, S> {
    backends: HashMap<KeyId, S>,
    selection: Selection<K>,

    /// Stores the index of the backend that has been polled to ready. The
    /// service at this index will be used on the next invocation of
    /// `Service::call`.
    ready_idx: Option<KeyId>,
}

/// Holds per-distribution state for a [`Distribute`] service.
#[derive(Debug)]
enum Selection<K> {
    Empty,
    FirstAvailable {
        keys: Arc<ServiceKeys<K>>,
    },
    RandomAvailable {
        keys: Arc<WeightedServiceKeys<K>>,
        rng: SmallRng,
    },
}

// === impl Distribute ===

impl<K: Hash + Eq, S> Distribute<K, S> {
    pub(crate) fn new<N>(dist: Distribution<K>, make_svc: N) -> Self
    where
        N: for<'a> NewService<&'a K, Service = S>,
    {
        let backends = Self::make_backends(&dist, make_svc);
        Self {
            backends,
            selection: dist.into(),
            ready_idx: None,
        }
    }

    fn make_backends<N>(dist: &Distribution<K>, make_svc: N) -> HashMap<KeyId, S>
    where
        N: for<'a> NewService<&'a K, Service = S>,
    {
        // Build the backends needed for this distribution, in the required
        // order (so that weighted indices align).
        match dist {
            Distribution::Empty => HashMap::new(),
            Distribution::FirstAvailable(keys) => keys
                .iter()
                .map(|&id| (id, make_svc.new_service(keys.get(id))))
                .collect(),
            Distribution::RandomAvailable(keys) => keys
                .iter()
                .map(|&id| (id, make_svc.new_service(&keys.get(id).key)))
                .collect(),
        }
    }
}

impl<Req, K, S> Service<Req> for Distribute<K, S>
where
    K: Hash + Eq,
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    /// Acquires a ready backend.
    ///
    /// Note that this doesn't necessarily drive all backend services to
    /// readiness. We expect that these inner services should be buffered or
    /// otherwise drive themselves to readiness (i.e. via SpawnReady).
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // If we've already chosen a ready index, then skip polling.
        if self.ready_idx.is_some() {
            return Poll::Ready(Ok(()));
        }

        match self.selection {
            Selection::Empty => {
                tracing::debug!("empty distribution will never become ready");
            }

            Selection::FirstAvailable { ref keys } => {
                for id in keys.iter() {
                    let svc = self
                        .backends
                        .get_mut(id)
                        .expect("distributions must not reference unknown backends");
                    if svc.poll_ready(cx)?.is_ready() {
                        self.ready_idx = Some(*id);
                        return Poll::Ready(Ok(()));
                    }
                }
            }

            // Choose a random index (via the weighted distribution) to try to
            // poll the backend. Continue selecting endpoints until we find one
            // that is ready or we've tried all backends in the distribution.
            Selection::RandomAvailable {
                ref keys,
                ref mut rng,
            } => {
                let mut selector = keys.selector();
                loop {
                    let id = selector.select_weighted(rng);
                    let svc = self
                        .backends
                        .get_mut(&id)
                        .expect("distributions must not reference unknown backends");

                    if svc.poll_ready(cx)?.is_ready() {
                        self.ready_idx = Some(id);
                        return Poll::Ready(Ok(()));
                    }

                    // Since the backend we just tried isn't ready, zero out the weight
                    // so that it's not tried again in this round, i.e. subsequent calls
                    // to `poll_ready` can try this backend again.
                    match selector.disable_backend(id) {
                        Ok(()) => {}
                        Err(WeightedError::AllWeightsZero) => {
                            // There are no backends remaining.
                            break;
                        }
                        Err(error) => {
                            tracing::error!(%error, "unexpected error updating weights; giving up");
                            break;
                        }
                    }
                }
            }
        }

        debug_assert!(self.ready_idx.is_none());
        tracing::trace!("no ready services in distribution");
        Poll::Pending
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let id = self
            .ready_idx
            .take()
            .expect("poll_ready must be called first");

        let svc = self.backends.get_mut(&id).expect("index must exist");

        svc.call(req)
    }
}

impl<K: Clone, S: Clone> Clone for Distribute<K, S> {
    fn clone(&self) -> Self {
        Self {
            backends: self.backends.clone(),
            selection: self.selection.clone(),
            // Clear the ready index so that the new clone must become ready
            // independently.
            ready_idx: None,
        }
    }
}

impl<K, S> Default for Distribute<K, S> {
    /// Returns an empty distribution. This distribution will never become
    /// ready.
    fn default() -> Self {
        Self {
            backends: Default::default(),
            selection: Selection::Empty,
            ready_idx: None,
        }
    }
}

// === impl Selection ===

impl<K> From<Distribution<K>> for Selection<K> {
    fn from(dist: Distribution<K>) -> Self {
        match dist {
            Distribution::Empty => Self::Empty,
            Distribution::FirstAvailable(keys) => Self::FirstAvailable { keys },
            Distribution::RandomAvailable(keys) => Self::RandomAvailable {
                keys,
                rng: SmallRng::from_rng(rand::thread_rng()).expect("RNG must initialize"),
            },
        }
    }
}

impl<K> Clone for Selection<K> {
    fn clone(&self) -> Self {
        match self {
            Self::Empty => Selection::Empty,
            Self::FirstAvailable { keys } => Self::FirstAvailable { keys: keys.clone() },
            Self::RandomAvailable { keys, .. } => Self::RandomAvailable {
                keys: keys.clone(),
                rng: SmallRng::from_rng(rand::thread_rng()).expect("RNG must initialize"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use tokio_test::*;
    use tower_test::mock;

    fn mock_first_available<K: Clone + PartialEq + Eq + Hash, S>(
        svcs: Vec<(K, S)>,
    ) -> Distribute<K, S> {
        // Wrap in RefCell because NewService is only blanked impl'd for Fn, not FnMut
        let svcs = RefCell::new(svcs);
        let dist = Distribution::first_available(svcs.borrow().iter().map(|(k, _)| k.clone()));
        let dist = Distribute::new(dist, |_: &K| svcs.borrow_mut().remove(0).1);
        assert!(svcs.borrow().is_empty());
        dist
    }

    fn mock_random_available<K: Clone + PartialEq + Eq + Hash, S>(
        svcs: Vec<(K, S, u32)>,
    ) -> Distribute<K, S> {
        let svcs = RefCell::new(svcs);
        let dist = Distribution::random_available(
            svcs.borrow()
                .iter()
                .map(|(k, _, weight)| (k.clone(), *weight)),
        )
        .unwrap();
        let dist = Distribute::new(dist, |_: &K| svcs.borrow_mut().remove(0).1);
        assert!(svcs.borrow().is_empty());
        dist
    }

    #[test]
    fn empty_pending() {
        let mut dist_svc = mock::Spawn::new(Distribute::<&'static str, mock::Mock<(), ()>>::new(
            Default::default(),
            |_: &&str| panic!("Empty service should never call make_svc"),
        ));
        assert_eq!(dist_svc.get_ref().backends.len(), 0);
        assert_pending!(dist_svc.poll_ready());
    }

    #[test]
    fn first_available_woken() {
        let (mulder, mut mulder_ctl) = mock::pair::<(), ()>();
        let (scully, mut scully_ctl) = mock::pair::<(), ()>();
        let mut dist_svc = mock::Spawn::new(mock_first_available(vec![
            ("mulder", mulder),
            ("scully", scully),
        ]));

        mulder_ctl.allow(0);
        scully_ctl.allow(0);
        assert_pending!(dist_svc.poll_ready());
        scully_ctl.allow(1);
        assert!(dist_svc.is_woken());
    }

    #[test]
    fn first_available_prefers_first() {
        let (mulder, mut mulder_ctl) = mock::pair();
        let (scully, mut scully_ctl) = mock::pair();
        let mut dist_svc = mock::Spawn::new(mock_first_available(vec![
            ("mulder", mulder),
            ("scully", scully),
        ]));

        scully_ctl.allow(1);
        mulder_ctl.allow(1);
        assert_ready_ok!(dist_svc.poll_ready());
        assert_eq!(dist_svc.get_ref().ready_idx, Some(KeyId::new(0)));
        let mut call = task::spawn(dist_svc.call(()));
        match assert_ready!(mulder_ctl.poll_request()) {
            Some(((), rsp)) => rsp.send_response(()),
            _ => panic!("expected request"),
        }
        assert_ready_ok!(call.poll());
    }

    #[test]
    fn first_available_uses_second() {
        let (mulder, mut mulder_ctl) = mock::pair();
        let (scully, mut scully_ctl) = mock::pair();
        let mut dist_svc = mock::Spawn::new(mock_first_available(vec![
            ("mulder", mulder),
            ("scully", scully),
        ]));

        mulder_ctl.allow(0);
        scully_ctl.allow(1);
        assert_ready_ok!(dist_svc.poll_ready());
        assert_eq!(dist_svc.get_ref().ready_idx, Some(KeyId::new(1)));
        let mut call = task::spawn(dist_svc.call(()));
        match assert_ready!(scully_ctl.poll_request()) {
            Some(((), rsp)) => rsp.send_response(()),
            _ => panic!("expected request"),
        }
        assert_ready_ok!(call.poll());
    }

    #[test]
    fn first_available_duplicate_keys() {
        let (mulder_1, mut mulder_1_ctl) = mock::pair();
        let (mulder_2, mut mulder_2_ctl) = mock::pair();
        let mut dist_svc = mock::Spawn::new(mock_first_available(vec![
            ("mulder", mulder_1),
            ("mulder", mulder_2),
        ]));

        mulder_2_ctl.allow(1);
        mulder_1_ctl.allow(1);
        assert_ready_ok!(dist_svc.poll_ready());
        assert_eq!(dist_svc.get_ref().ready_idx, Some(KeyId::new(0)));
        let mut call = task::spawn(dist_svc.call(()));
        match assert_ready!(mulder_1_ctl.poll_request()) {
            Some(((), rsp)) => rsp.send_response(()),
            _ => panic!("expected request"),
        }
        assert_ready_ok!(call.poll());
    }

    #[test]
    fn random_available_woken() {
        let (mulder, mut mulder_ctl) = mock::pair::<(), ()>();
        let (scully, mut scully_ctl) = mock::pair::<(), ()>();
        let (skinner, mut skinner_ctl) = mock::pair::<(), ()>();
        let mut dist_svc = mock::Spawn::new(mock_random_available(vec![
            ("mulder", mulder, 1),
            ("scully", scully, 99998),
            ("skinner", skinner, 1),
        ]));

        mulder_ctl.allow(0);
        scully_ctl.allow(0);
        skinner_ctl.allow(0);
        assert_pending!(dist_svc.poll_ready());
        skinner_ctl.allow(1);
        assert!(dist_svc.is_woken());
    }

    #[test]
    fn random_available_follows_weight() {
        let (mulder, mut mulder_ctl) = mock::pair();
        let (scully, mut scully_ctl) = mock::pair();
        let (skinner, mut skinner_ctl) = mock::pair();
        let mut dist_svc = mock::Spawn::new(mock_random_available(vec![
            ("mulder", mulder, 1),
            ("scully", scully, 99998),
            ("skinner", skinner, 1),
        ]));

        mulder_ctl.allow(1);
        scully_ctl.allow(1);
        skinner_ctl.allow(1);
        assert_ready_ok!(dist_svc.poll_ready());
        assert_eq!(dist_svc.get_ref().ready_idx, Some(KeyId::new(1)));
        let mut call = task::spawn(dist_svc.call(()));
        match assert_ready!(scully_ctl.poll_request()) {
            Some(((), rsp)) => rsp.send_response(()),
            _ => panic!("expected request"),
        }
        assert_ready_ok!(call.poll());
    }

    #[test]
    fn random_available_follows_availability() {
        let (mulder, mut mulder_ctl) = mock::pair();
        let (scully, mut scully_ctl) = mock::pair();
        let (skinner, mut skinner_ctl) = mock::pair();
        let mut dist_svc = mock::Spawn::new(mock_random_available(vec![
            ("mulder", mulder, 1),
            ("scully", scully, 99998),
            ("skinner", skinner, 1),
        ]));

        mulder_ctl.allow(1);
        scully_ctl.allow(0);
        skinner_ctl.allow(0);
        assert_ready_ok!(dist_svc.poll_ready());
        assert_eq!(dist_svc.get_ref().ready_idx, Some(KeyId::new(0)));
        let mut call = task::spawn(dist_svc.call(()));
        match assert_ready!(mulder_ctl.poll_request()) {
            Some(((), rsp)) => rsp.send_response(()),
            _ => panic!("expected request"),
        }
        assert_ready_ok!(call.poll());
    }

    #[test]
    fn random_available_duplicate_keys_allowed() {
        let (mulder_1, mut mulder_1_ctl) = mock::pair();
        let (mulder_2, mut mulder_2_ctl) = mock::pair();
        let (mulder_3, mut mulder_3_ctl) = mock::pair();
        let mut dist_svc = mock::Spawn::new(mock_random_available(vec![
            ("mulder", mulder_1, 1),
            ("mulder", mulder_2, 99998),
            ("mulder", mulder_3, 1),
        ]));

        mulder_1_ctl.allow(1);
        mulder_2_ctl.allow(1);
        mulder_3_ctl.allow(1);
        assert_ready_ok!(dist_svc.poll_ready());
        assert_eq!(dist_svc.get_ref().ready_idx, Some(KeyId::new(1)));
        let mut call = task::spawn(dist_svc.call(()));
        match assert_ready!(mulder_2_ctl.poll_request()) {
            Some(((), rsp)) => rsp.send_response(()),
            _ => panic!("expected request"),
        }
        assert_ready_ok!(call.poll());
    }
}
