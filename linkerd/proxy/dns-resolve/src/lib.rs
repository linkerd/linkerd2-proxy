#![recursion_limit = "512"]

use async_stream::try_stream;
use futures::future;
use futures::pin_mut;
use futures::prelude::*;
use linkerd2_addr::NameAddr;
use linkerd2_dns as dns;
use linkerd2_error::Error;
use linkerd2_proxy_core::resolve::Update;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::debug;

#[derive(Clone)]
pub struct DnsResolve {
    dns: linkerd2_dns::Resolver,
}

impl DnsResolve {
    pub fn new(dns: dns::Resolver) -> Self {
        Self { dns }
    }
}

type UpdatesStream =
    Pin<Box<dyn Stream<Item = Result<Update<SocketAddr>, Error>> + Send + Sync + 'static>>;

impl tower::Service<NameAddr> for DnsResolve {
    type Response = UpdatesStream;
    type Error = Error;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: NameAddr) -> Self::Future {
        futures::future::ok(Box::pin(resolution_stream(
            self.dns.resolve_service_ips(target.name()),
        )))
    }
}

pub fn diff_targets(
    old_targets: &Vec<SocketAddr>,
    new_targets: &Vec<SocketAddr>,
) -> (Vec<SocketAddr>, Vec<SocketAddr>) {
    let mut adds = Vec::default();
    let mut removes = Vec::default();

    for new_target in new_targets {
        if !old_targets.contains(new_target) {
            adds.push(new_target.clone())
        }
    }

    for old_target in old_targets {
        if !new_targets.contains(old_target) {
            removes.push(old_target.clone())
        }
    }

    (adds, removes)
}

pub fn resolution_stream<S>(input: S) -> impl Stream<Item = Result<Update<SocketAddr>, Error>>
where
    S: futures::stream::Stream<Item = Result<Vec<SocketAddr>, dns::Error>>,
    S: Send + 'static,
{
    try_stream! {
        pin_mut!(input);
        let mut current: Vec<SocketAddr> = Vec::new();
        while let Some(result) = input.next().await {
            let result = result?;
            if result.is_empty() {
                current.clear();
                yield Update::Reset(Vec::new());
            }

            let (adds, removes) = diff_targets(&current, &result);
            debug!(?adds, ?removes, "resolved");
            if !adds.is_empty() {
                let adds = adds.into_iter().map(|addr| (addr, addr)).collect();
                yield Update::Add(adds);
            }

            if !removes.is_empty() {
                yield Update::Remove(removes);
            }
            current.clear();
            current.extend(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio_test::{assert_pending, assert_ready_eq, task};

    #[tokio::test]
    async fn yields_add() {
        let (mut tx, rx1) = mpsc::channel(10);

        let mut stream = task::spawn(resolution_stream(rx1).map_err(|_| ()));

        let sa = "127.0.0.1:8080".parse().unwrap();
        assert_pending!(stream.poll_next());
        tx.send(Ok(vec![sa])).await.unwrap();
        assert_ready_eq!(stream.poll_next(), Some(Ok(Update::Add(vec![(sa, sa)]))));
    }

    #[tokio::test]
    async fn yields_remove() {
        let (mut tx, rx1) = mpsc::channel(10);

        let mut stream = task::spawn(resolution_stream(rx1).map_err(|_| ()));

        let sa_1 = "127.0.0.1:8080".parse().unwrap();
        let sa_2 = "127.0.0.2:8080".parse().unwrap();

        tx.send(Ok(vec![sa_1])).await.unwrap();
        assert_ready_eq!(
            stream.poll_next(),
            Some(Ok(Update::Add(vec![(sa_1, sa_1)])))
        );
        tx.send(Ok(vec![sa_2])).await.unwrap();
        assert_ready_eq!(
            stream.poll_next(),
            Some(Ok(Update::Add(vec![(sa_2, sa_2)])))
        );
        assert_ready_eq!(stream.poll_next(), Some(Ok(Update::Remove(vec![sa_1]))));
    }

    #[tokio::test]
    async fn yields_empty() {
        let (mut tx, rx1) = mpsc::channel(10);

        let mut stream = task::spawn(resolution_stream(rx1).map_err(|_| ()));

        let sa_1 = "127.0.0.1:8080".parse().unwrap();
        let sa_2 = "127.0.0.2:8080".parse().unwrap();

        tx.send(Ok(vec![sa_1, sa_2])).await.unwrap();
        assert_ready_eq!(
            stream.poll_next(),
            Some(Ok(Update::Add(vec![(sa_1, sa_1), (sa_2, sa_2)])))
        );
        tx.send(Ok(vec![])).await.unwrap();
        assert_ready_eq!(stream.poll_next(), Some(Ok(Update::Empty)));
    }
}
