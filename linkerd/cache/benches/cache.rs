#![feature(test)]
use futures::future;
use linkerd2_cache::{Cache, Handle};
use linkerd2_error::Never;
use linkerd2_stack::NewService;
use std::task::{Context, Poll};
use tower::util::ServiceExt;
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
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, rhs: usize) -> Self::Future {
        future::ok(self.val * rhs)
    }
}

impl NewService<(usize, Handle)> for NewNeverEvict {
    type Service = NeverEvict;

    fn new_service(&mut self, target: (usize, Handle)) -> Self::Service {
        NeverEvict {
            val: target.0,
            handle: target.1,
        }
    }
}

impl Service<usize> for AlwaysEvict {
    type Response = usize;
    type Error = Never;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, rhs: usize) -> Self::Future {
        future::ok(self.val * rhs)
    }
}

impl NewService<(usize, Handle)> for NewAlwaysEvict {
    type Service = AlwaysEvict;

    fn new_service(&mut self, target: (usize, Handle)) -> Self::Service {
        AlwaysEvict { val: target.0 }
    }
}

impl Service<usize> for SometimesEvict {
    type Response = usize;
    type Error = Never;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, rhs: usize) -> Self::Future {
        future::ok(self.val * rhs)
    }
}

impl NewService<(usize, Handle)> for NewSometimesEvict {
    type Service = SometimesEvict;

    fn new_service(&mut self, target: (usize, Handle)) -> Self::Service {
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
    N::Service: Clone + Send + 'static,
    <N::Service as Service<usize>>::Error: std::fmt::Debug,
{
    let mut cache = Cache::new(new_svc);
    b.iter(|| {
        for n in 0..num_svc {
            futures::executor::block_on(async {
                let mut svc = cache.ready_and().await.unwrap();
                svc.call(n).await.unwrap().call(n).await.unwrap();
            })
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
