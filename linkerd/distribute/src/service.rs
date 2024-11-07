use self::{first::FirstAvailableSelection, random::RandomAvailableSelection};
use super::Distribution;
use linkerd_stack::{NewService, Service};
use std::{
    hash::Hash,
    task::{Context, Poll},
};

mod first;
mod random;

/// A service that distributes requests over a set of backends.
#[derive(Debug, Clone)]
pub struct Distribute<K, S> {
    selection: Selection<K, S>,
}

/// Holds per-distribution state for a [`Distribute`] service.
#[derive(Debug)]
enum Selection<K, S> {
    Empty,
    FirstAvailable(FirstAvailableSelection<S>),
    RandomAvailable(RandomAvailableSelection<K, S>),
}

// === impl Distribute ===

impl<K: Hash + Eq, S> Distribute<K, S> {
    pub(crate) fn new<N>(dist: Distribution<K>, make_svc: N) -> Self
    where
        N: for<'a> NewService<&'a K, Service = S>,
    {
        Self {
            selection: Self::make_selection(&dist, make_svc),
        }
    }

    fn make_selection<N>(dist: &Distribution<K>, make_svc: N) -> Selection<K, S>
    where
        N: for<'a> NewService<&'a K, Service = S>,
    {
        // Build the backends needed for this distribution, in the required
        // order (so that weighted indices align).
        match dist {
            Distribution::Empty => Selection::Empty,
            Distribution::FirstAvailable(keys) => {
                Selection::FirstAvailable(FirstAvailableSelection::new(keys, make_svc))
            }
            Distribution::RandomAvailable(keys) => {
                Selection::RandomAvailable(RandomAvailableSelection::new(keys, make_svc))
            }
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
        match &mut self.selection {
            Selection::Empty => {
                tracing::debug!("empty distribution will never become ready");
                Poll::Pending
            }
            Selection::FirstAvailable(s) => s.poll_ready(cx),
            Selection::RandomAvailable(s) => s.poll_ready(cx),
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match &mut self.selection {
            Selection::Empty => unreachable!("Empty selection is never ready"),
            Selection::FirstAvailable(s) => s.call(req),
            Selection::RandomAvailable(s) => s.call(req),
        }
    }
}

// === impl Selection ===

impl<K, S> Default for Selection<K, S> {
    /// Returns an empty distribution. This distribution will never become
    /// ready.
    fn default() -> Self {
        Self::Empty
    }
}

impl<K, S: Clone> Clone for Selection<K, S> {
    fn clone(&self) -> Self {
        match self {
            Self::Empty => Self::Empty,
            Self::FirstAvailable(s) => Self::FirstAvailable(s.clone()),
            Self::RandomAvailable(s) => Self::RandomAvailable(s.clone()),
        }
    }
}

impl<K, S> Default for Distribute<K, S> {
    fn default() -> Self {
        Self {
            selection: Selection::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use crate::keys::KeyId;

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
        assert!(matches!(dist_svc.get_ref().selection, Selection::Empty));
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
        let Selection::FirstAvailable(selection) = &dist_svc.get_ref().selection else {
            panic!()
        };
        assert_eq!(selection.get_ready_idx(), Some(0));
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
        let Selection::FirstAvailable(selection) = &dist_svc.get_ref().selection else {
            panic!()
        };
        assert_eq!(selection.get_ready_idx(), Some(1));
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
        let Selection::FirstAvailable(selection) = &dist_svc.get_ref().selection else {
            panic!()
        };
        assert_eq!(selection.get_ready_idx(), Some(0));
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
        let Selection::RandomAvailable(selection) = &dist_svc.get_ref().selection else {
            panic!()
        };
        assert_eq!(selection.get_ready_idx(), Some(KeyId::new(1)));
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
        let Selection::RandomAvailable(selection) = &dist_svc.get_ref().selection else {
            panic!()
        };
        assert_eq!(selection.get_ready_idx(), Some(KeyId::new(0)));
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
        let Selection::RandomAvailable(selection) = &dist_svc.get_ref().selection else {
            panic!()
        };
        assert_eq!(selection.get_ready_idx(), Some(KeyId::new(1)));
        let mut call = task::spawn(dist_svc.call(()));
        match assert_ready!(mulder_2_ctl.poll_request()) {
            Some(((), rsp)) => rsp.send_response(()),
            _ => panic!("expected request"),
        }
        assert_ready_ok!(call.poll());
    }
}
