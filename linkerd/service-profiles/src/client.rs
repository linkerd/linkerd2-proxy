use crate::{http, Profile, Receiver, Target};
use api::destination_client::DestinationClient;
use futures::{future, prelude::*, ready, select_biased};
use http_body::Body as HttpBody;
use linkerd2_proxy_api::destination as api;
use linkerd_addr::Addr;
use linkerd_dns_name::Name;
use linkerd_error::{Error, Recover};
use linkerd_proxy_api_resolve::pb as resolve;
use pin_project::pin_project;
use regex::Regex;
use std::{
    convert::TryInto,
    future::Future,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::watch;
use tonic::{
    self as grpc,
    body::{Body, BoxBody},
    client::GrpcService,
};
use tower::retry::budget::Budget;
use tracing::{debug, debug_span, error, trace, warn};
use tracing_futures::Instrument;

#[derive(Clone, Debug)]
pub struct Client<S, R> {
    service: DestinationClient<S>,
    recover: R,
    context_token: String,
}

#[pin_project]
pub struct ProfileFuture<S, R>
where
    S: GrpcService<BoxBody>,
    R: Recover,
{
    #[pin]
    inner: Option<Inner<S, R>>,
}

#[pin_project]
struct Inner<S, R>
where
    S: GrpcService<BoxBody>,
    R: Recover,
{
    service: DestinationClient<S>,
    recover: R,
    #[pin]
    state: State<R::Backoff>,
    request: api::GetDestination,
}

#[pin_project(project = StateProj)]
enum State<B> {
    Disconnected {
        backoff: Option<B>,
    },
    Waiting {
        future: RspFuture,
        backoff: Option<B>,
    },
    Streaming(#[pin] grpc::Streaming<api::DestinationProfile>),
    Backoff(Option<B>),
}

type RspFuture = Pin<
    Box<
        dyn Future<
                Output = Result<
                    tonic::Response<tonic::Streaming<api::DestinationProfile>>,
                    grpc::Status,
                >,
            > + Send
            + 'static,
    >,
>;

// === impl Client ===

impl<S, R> Client<S, R>
where
    // These bounds aren't *required* here, they just help detect the problem
    // earlier (as Client::new), instead of when trying to passing a `Client`
    // to something that wants `impl GetProfile`.
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as HttpBody>::Error:
        Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
    R: Recover,
    R::Backoff: Unpin,
{
    pub fn new(service: S, recover: R, context_token: String) -> Self {
        Self {
            service: DestinationClient::new(service),
            recover,
            context_token,
        }
    }
}

impl<T, S, R> tower::Service<T> for Client<S, R>
where
    T: ToString,
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as HttpBody>::Error:
        Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
    R: Recover + Send + Clone + 'static,
    R::Backoff: Unpin + Send,
{
    type Response = Option<Receiver>;
    type Error = Error;
    type Future = ProfileFuture<S, R>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Tonic will internally drive the client service to readiness.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, t: T) -> Self::Future {
        let request = api::GetDestination {
            path: t.to_string(),
            context_token: self.context_token.clone(),
            ..Default::default()
        };

        let inner = Inner {
            request,
            service: self.service.clone(),
            recover: self.recover.clone(),
            state: State::Disconnected { backoff: None },
        };
        ProfileFuture { inner: Some(inner) }
    }
}

impl<S, R> Future for ProfileFuture<S, R>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as HttpBody>::Error: Into<Error> + Send,
    S::Future: Send,
    R: Recover + Send + 'static,
    R::Backoff: Unpin,
    R::Backoff: Send,
{
    type Output = Result<Option<Receiver>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let profile = match this
            .inner
            .as_mut()
            .as_pin_mut()
            .expect("polled after ready")
            .poll_profile(cx)
        {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(error)) => {
                trace!(%error, "failed to fetch profile");
                return Poll::Ready(Err(error));
            }
            Poll::Ready(Ok(profile)) => profile,
        };

        trace!("daemonizing");
        let (tx, rx) = watch::channel(profile);
        let inner = this.inner.take().expect("polled after ready");
        let daemon = async move {
            tokio::pin!(inner);
            loop {
                select_biased! {
                    _ = tx.closed().fuse() => {
                        trace!("profile observation dropped");
                        return;
                    },
                    profile = future::poll_fn(|cx|
                        inner.as_mut().poll_profile(cx)
                        ).fuse() => {
                        match profile {
                            Err(error) => {
                                error!(%error, "profile client died");
                                return;
                            }
                            Ok(profile) => {
                                trace!(?profile, "publishing");
                                if tx.send(profile).is_err() {
                                    trace!("failed to publish profile");
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        };
        tokio::spawn(daemon.in_current_span());

        Poll::Ready(Ok(Some(rx)))
    }
}

// === impl Inner ===

impl<S, R> Inner<S, R>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send + 'static,
    <S::ResponseBody as HttpBody>::Error: Into<Error> + Send,
    S::Future: Send,
    R: Recover,
    R::Backoff: Unpin,
{
    fn poll_rx(
        rx: Pin<&mut grpc::Streaming<api::DestinationProfile>>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Profile, grpc::Status>>> {
        trace!("poll");
        let profile = ready!(rx.poll_next(cx)).map(|res| {
            res.map(|proto| {
                debug!("profile received: {:?}", proto);
                let name = Name::from_str(&proto.fully_qualified_name).ok();
                let retry_budget = proto.retry_budget.and_then(convert_retry_budget);
                let http_routes = proto
                    .routes
                    .into_iter()
                    .filter_map(move |orig| convert_route(orig, retry_budget.as_ref()))
                    .collect();
                let targets = proto
                    .dst_overrides
                    .into_iter()
                    .filter_map(convert_dst_override)
                    .collect();
                let endpoint = proto.endpoint.and_then(|e| {
                    let labels = std::collections::HashMap::new();
                    resolve::to_addr_meta(e, &labels)
                });
                Profile {
                    name,
                    http_routes,
                    targets,
                    opaque_protocol: proto.opaque_protocol,
                    endpoint,
                }
            })
        });
        Poll::Ready(profile)
    }

    fn poll_profile(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Profile, Error>> {
        let span = debug_span!("poll_profile");
        let _enter = span.enter();

        loop {
            let mut this = self.as_mut().project();
            match this.state.as_mut().project() {
                StateProj::Disconnected { backoff } => {
                    trace!("disconnected");
                    let mut svc = this.service.clone();
                    let req = this.request.clone();
                    let future =
                        Box::pin(async move { svc.get_profile(grpc::Request::new(req)).await });
                    let backoff = backoff.take();
                    this.state.as_mut().set(State::Waiting { future, backoff });
                }
                StateProj::Waiting { future, backoff } => {
                    trace!("waiting");
                    match ready!(Pin::new(future).poll(cx)) {
                        Ok(rsp) => this.state.set(State::Streaming(rsp.into_inner())),
                        Err(e) => {
                            let error = e.into();
                            warn!(%error, "Could not fetch profile");
                            let new_backoff = this.recover.recover(error)?;
                            let backoff = Some(backoff.take().unwrap_or(new_backoff));
                            this.state.set(State::Disconnected { backoff });
                        }
                    }
                }
                StateProj::Streaming(s) => {
                    trace!("streaming");
                    let status = match ready!(Self::poll_rx(s, cx)) {
                        Some(Ok(profile)) => return Poll::Ready(Ok(profile)),
                        None => grpc::Status::new(grpc::Code::Ok, ""),
                        Some(Err(status)) => status,
                    };
                    trace!(?status);
                    let backoff = this.recover.recover(status.into())?;
                    this.state.set(State::Backoff(Some(backoff)));
                }
                StateProj::Backoff(ref mut backoff) => {
                    trace!("backoff");
                    let backoff = match ready!(backoff.as_mut().unwrap().poll_next_unpin(cx)) {
                        Some(()) => backoff.take(),
                        None => None,
                    };
                    this.state.set(State::Disconnected { backoff });
                }
            };
        }
    }
}

fn convert_route(
    orig: api::Route,
    retry_budget: Option<&Arc<Budget>>,
) -> Option<(http::RequestMatch, http::Route)> {
    let req_match = orig.condition.and_then(convert_req_match)?;
    let rsp_classes = orig
        .response_classes
        .into_iter()
        .filter_map(convert_rsp_class)
        .collect();
    let mut route = http::Route::new(orig.metrics_labels.into_iter(), rsp_classes);
    if orig.is_retryable {
        set_route_retry(&mut route, retry_budget);
    }
    if let Some(timeout) = orig.timeout {
        set_route_timeout(&mut route, timeout.try_into());
    }
    Some((req_match, route))
}

fn convert_dst_override(orig: api::WeightedDst) -> Option<Target> {
    if orig.weight == 0 {
        return None;
    }
    let addr = Addr::from_str(orig.authority.as_str()).ok()?;
    Some(Target {
        addr,
        weight: orig.weight,
    })
}

fn set_route_retry(route: &mut http::Route, retry_budget: Option<&Arc<Budget>>) {
    let budget = match retry_budget {
        Some(budget) => budget.clone(),
        None => {
            warn!("retry_budget is missing: {:?}", route);
            return;
        }
    };

    route.set_retries(budget);
}

fn set_route_timeout(route: &mut http::Route, timeout: Result<Duration, Duration>) {
    match timeout {
        Ok(dur) => {
            route.set_timeout(dur);
        }
        Err(_) => {
            warn!("route timeout is negative: {:?}", route);
        }
    }
}

fn convert_req_match(orig: api::RequestMatch) -> Option<http::RequestMatch> {
    let m = match orig.r#match? {
        api::request_match::Match::All(ms) => {
            let ms = ms.matches.into_iter().filter_map(convert_req_match);
            http::RequestMatch::All(ms.collect())
        }
        api::request_match::Match::Any(ms) => {
            let ms = ms.matches.into_iter().filter_map(convert_req_match);
            http::RequestMatch::Any(ms.collect())
        }
        api::request_match::Match::Not(m) => {
            let m = convert_req_match(*m)?;
            http::RequestMatch::Not(Box::new(m))
        }
        api::request_match::Match::Path(api::PathMatch { regex }) => {
            let regex = regex.trim();
            let re = match (regex.starts_with('^'), regex.ends_with('$')) {
                (true, true) => Regex::new(regex).ok()?,
                (hd_anchor, tl_anchor) => {
                    let hd = if hd_anchor { "" } else { "^" };
                    let tl = if tl_anchor { "" } else { "$" };
                    let re = format!("{}{}{}", hd, regex, tl);
                    Regex::new(&re).ok()?
                }
            };
            http::RequestMatch::Path(re)
        }
        api::request_match::Match::Method(mm) => {
            let m = mm.r#type.and_then(|m| (&m).try_into().ok())?;
            http::RequestMatch::Method(m)
        }
    };

    Some(m)
}

fn convert_rsp_class(orig: api::ResponseClass) -> Option<http::ResponseClass> {
    let c = orig.condition.and_then(convert_rsp_match)?;
    Some(http::ResponseClass::new(orig.is_failure, c))
}

fn convert_rsp_match(orig: api::ResponseMatch) -> Option<http::ResponseMatch> {
    let m = match orig.r#match? {
        api::response_match::Match::All(ms) => {
            let ms = ms
                .matches
                .into_iter()
                .filter_map(convert_rsp_match)
                .collect::<Vec<_>>();
            if ms.is_empty() {
                return None;
            }
            http::ResponseMatch::All(ms)
        }
        api::response_match::Match::Any(ms) => {
            let ms = ms
                .matches
                .into_iter()
                .filter_map(convert_rsp_match)
                .collect::<Vec<_>>();
            if ms.is_empty() {
                return None;
            }
            http::ResponseMatch::Any(ms)
        }
        api::response_match::Match::Not(m) => {
            let m = convert_rsp_match(*m)?;
            http::ResponseMatch::Not(Box::new(m))
        }
        api::response_match::Match::Status(range) => {
            let min = ::http::StatusCode::from_u16(range.min as u16).ok()?;
            let max = ::http::StatusCode::from_u16(range.max as u16).ok()?;
            http::ResponseMatch::Status { min, max }
        }
    };

    Some(m)
}

fn convert_retry_budget(orig: api::RetryBudget) -> Option<Arc<Budget>> {
    let min_retries = if orig.min_retries_per_second <= ::std::i32::MAX as u32 {
        orig.min_retries_per_second
    } else {
        warn!(
            "retry_budget min_retries_per_second overflow: {:?}",
            orig.min_retries_per_second
        );
        return None;
    };

    let retry_ratio = orig.retry_ratio;
    if !(0.0..=1000.0).contains(&retry_ratio) {
        warn!("retry_budget retry_ratio invalid: {:?}", retry_ratio);
        return None;
    }

    let ttl = match orig.ttl {
        Some(pb_dur) => {
            if pb_dur.seconds < 0 || pb_dur.nanos < 0 {
                warn!("retry_budget ttl negative");
                return None;
            }
            match pb_dur.try_into() {
                Ok(dur) => {
                    if dur > Duration::from_secs(60) || dur < Duration::from_secs(1) {
                        warn!("retry_budget ttl invalid: {:?}", dur);
                        return None;
                    }
                    dur
                }
                Err(negative) => {
                    warn!("retry_budget ttl negative: {:?}", negative);
                    return None;
                }
            }
        }
        None => {
            warn!("retry_budget ttl missing");
            return None;
        }
    };

    Some(Arc::new(Budget::new(ttl, min_retries, retry_ratio)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck::*;

    quickcheck! {
        fn retry_budget_from_proto(
            min_retries_per_second: u32,
            retry_ratio: f32,
            seconds: i64,
            nanos: i32
        ) -> TestResult {
            // if seconds < 0 || nanos < 0 || nanos > 1_000_000_000 {
            //     return TestResult::discard();
            // }
            let proto = api::RetryBudget {
                min_retries_per_second,
                retry_ratio,
                ttl: Some(prost_types::Duration {
                    seconds,
                    nanos: 0,
                }),
            };
            // Ensure we don't panic.
            convert_retry_budget(proto);
            TestResult::passed()
        }
    }
}
