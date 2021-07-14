use crate::{proto, LookupAddr, Profile, Receiver};
use futures::prelude::*;
use http_body::Body;
use linkerd2_proxy_api::destination::{self as api, destination_client::DestinationClient};
use linkerd_error::{Never, Recover};
use linkerd_stack::{Param, Service};
use linkerd_tonic_watch::StreamWatch;
use std::task::{Context, Poll};
use tonic::{body::BoxBody, client::GrpcService};
use tracing::debug;

/// Creates watches on service profiles.
#[derive(Clone, Debug)]
pub struct Client<R, S> {
    watch: StreamWatch<R, Inner<S>>,
}

/// Wraps the destination service to hide protobuf types.
#[derive(Clone, Debug)]
struct Inner<S> {
    client: DestinationClient<S>,
    context_token: String,
}

// === impl Client ===

impl<R, S> Client<R, S>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send + Sync,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as Body>::Error:
        Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
    R: Recover<tonic::Status> + Send + Clone + 'static,
    R::Backoff: Unpin + Send,
{
    pub fn new(recover: R, inner: S, context_token: String) -> Self {
        Self {
            watch: StreamWatch::new(recover, Inner::new(context_token, inner)),
        }
    }
}

impl<T, R, S> Service<T> for Client<R, S>
where
    T: Param<LookupAddr>,
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send + Sync,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as Body>::Error:
        Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
    R: Recover<tonic::Status> + Send + Clone + 'static,
    R::Backoff: Unpin + Send,
{
    type Response = Option<Receiver>;
    type Error = Never;
    type Future = futures::future::BoxFuture<'static, Result<Option<Receiver>, Never>>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, t: T) -> Self::Future {
        let addr = t.param();

        // Swallow errors in favor of a `None` response.
        let w = self.watch.clone();
        Box::pin(async move {
            match w.spawn_watch(addr).await {
                Ok(rsp) => {
                    let rx = rsp.into_inner();
                    Ok(Some(rx.into()))
                }
                Err(status) => {
                    debug!(%status, "Ignoring profile");
                    Ok::<_, Never>(None)
                }
            }
        })
    }
}

// === impl Inner ===

type InnerStream = futures::stream::BoxStream<'static, Result<Profile, tonic::Status>>;

type InnerFuture =
    futures::future::BoxFuture<'static, Result<tonic::Response<InnerStream>, tonic::Status>>;

impl<S> Inner<S>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send + Sync,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as Body>::Error:
        Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
{
    fn new(context_token: String, inner: S) -> Self {
        Self {
            context_token,
            client: DestinationClient::new(inner),
        }
    }
}

impl<S> Service<LookupAddr> for Inner<S>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send + Sync,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as Body>::Error:
        Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
{
    type Response = tonic::Response<InnerStream>;
    type Error = tonic::Status;
    type Future = InnerFuture;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Tonic clients do not expose readiness.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, LookupAddr(addr): LookupAddr) -> Self::Future {
        let req = api::GetDestination {
            path: addr.to_string(),
            context_token: self.context_token.clone(),
            ..Default::default()
        };

        let mut client = self.client.clone();
        Box::pin(async move {
            let rsp = client.get_profile(req).await?;
            Ok(rsp.map(|s| {
                Box::pin(s.map_ok(move |p| proto::convert_profile(p, addr.port()))) as InnerStream
            }))
        })
    }
}
