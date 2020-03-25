#![feature(test)]

extern crate linkerd2_cache;
use futures::{future, Async, Future, Poll};
use linkerd2_cache::{Cache, Handle};
use linkerd2_error::Never;
use linkerd2_stack::NewService;
use tower::Service;

extern crate test;
use test::Bencher;

#[derive(Clone, Debug)]
struct NeverEvict {
    val: usize,
    handle: Handle,
}

#[derive(Clone, Debug)]
struct NewNeverEvict;

#[derive(Clone, Debug)]
struct AlwaysEvict {
    val: usize,
}
#[derive(Clone, Debug)]
struct NewAlwaysEvict;

#[derive(Clone, Debug)]
struct SometimesEvict {
    val: usize,
    handle: Option<Handle>,
}

struct NewSometimesEvict;

impl Service<usize> for NeverEvict {
    type Response = usize;
    type Error = Never;
    type Future = futures::future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, rhs: usize) -> Self::Future {
        future::ok(self.val * rhs)
    }
}

impl NewService<(usize, Handle)> for NewNeverEvict {
    type Service = NeverEvict;

    fn new_service(&self, target: (usize, Handle)) -> Self::Service {
        NeverEvict {
            val: target.0,
            handle: target.1,
        }
    }
}

impl Service<usize> for AlwaysEvict {
    type Response = usize;
    type Error = Never;
    type Future = futures::future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, rhs: usize) -> Self::Future {
        future::ok(self.val * rhs)
    }
}

impl NewService<(usize, Handle)> for NewAlwaysEvict {
    type Service = AlwaysEvict;

    fn new_service(&self, target: (usize, Handle)) -> Self::Service {
        AlwaysEvict { val: target.0 }
    }
}

impl Service<usize> for SometimesEvict {
    type Response = usize;
    type Error = Never;
    type Future = futures::future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, rhs: usize) -> Self::Future {
        future::ok(self.val * rhs)
    }
}

impl NewService<(usize, Handle)> for NewSometimesEvict {
    type Service = SometimesEvict;

    fn new_service(&self, target: (usize, Handle)) -> Self::Service {
        if target.0 % 2 == 0 {
            SometimesEvict {
                val: target.0,
                handle: Some(target.1),
            }
        } else {
            SometimesEvict {
                val: target.0,
                handle: None,
            }
        }
    }
}

fn run_bench<N>(num_svc: usize, new_svc: N, b: &mut Bencher)
where
    N: NewService<(usize, Handle)>,
    N::Service: Service<usize>,
    N::Service: Clone,
{
    let mut cache = Cache::new(new_svc);
    b.iter(|| {
        for n in 0..num_svc {
            cache.poll_ready().unwrap();
            let _r = cache.call(n).wait().unwrap().call(n);
        }
    });
}

#[bench]
fn never_evict(b: &mut Bencher) {
    run_bench(100, NewNeverEvict, b)
}

#[bench]
fn always_evict(b: &mut Bencher) {
    run_bench(100, NewAlwaysEvict, b)
}

#[bench]
fn sometimes_evict(b: &mut Bencher) {
    run_bench(100, NewSometimesEvict, b)
}
