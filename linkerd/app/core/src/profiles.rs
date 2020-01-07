use crate::dns;
use crate::proxy::http::{profiles, retry::Budget};
use futures::{try_ready, Async, Future, Poll, Stream};
use http;
use linkerd2_addr::{Addr, NameAddr};
use linkerd2_error::{Error, Never, Recover};
use linkerd2_proxy_api::destination as api;
use regex::Regex;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::timer::Delay;
use tower_grpc::{self as grpc, generic::client::GrpcService, Body, BoxBody};
use tracing::{debug, error, info, info_span, trace, warn};
use tracing_futures::Instrument;

#[derive(Clone, Debug)]
pub struct Client<S, R> {
    service: api::client::Destination<S>,
    recover: R,
    initial_timeout: Duration,
    context_token: String,
    suffixes: Vec<dns::Suffix>,
}

pub type Receiver = watch::Receiver<profiles::Routes>;

type Sender = watch::Sender<profiles::Routes>;

pub struct ProfileFuture<S, R>
where
    S: GrpcService<BoxBody>,
    R: Recover,
{
    inner: ProfileFutureInner<S, R>,
}

enum ProfileFutureInner<S, R>
where
    S: GrpcService<BoxBody>,
    R: Recover,
{
    Disabled,
    Pending(Option<Inner<S, R>>, Delay),
}

struct Daemon<S, R>
where
    S: GrpcService<BoxBody>,
    R: Recover,
{
    tx: Sender,
    inner: Inner<S, R>,
}

struct Inner<S, R>
where
    S: GrpcService<BoxBody>,
    R: Recover,
{
    service: api::client::Destination<S>,
    recover: R,
    state: State<S, R::Backoff>,
    request: api::GetDestination,
}

enum State<S, B>
where
    S: GrpcService<BoxBody>,
{
    Disconnected {
        backoff: Option<B>,
    },
    Waiting {
        future: grpc::client::server_streaming::ResponseFuture<api::DestinationProfile, S::Future>,
        backoff: Option<B>,
    },
    Streaming(grpc::Streaming<api::DestinationProfile, S::ResponseBody>),
    Backoff(Option<B>),
}

// === impl Client ===

impl<S, R> Client<S, R>
where
    // These bounds aren't *required* here, they just help detect the problem
    // earlier (as Client::new), instead of when trying to passing a `Client`
    // to something that wants `impl profiles::GetRoutes`.
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    S::Future: Send,
    R: Recover,
{
    pub fn new(
        service: S,
        recover: R,
        initial_timeout: Duration,
        context_token: String,
        suffixes: impl IntoIterator<Item = dns::Suffix>,
    ) -> Self {
        Self {
            service: api::client::Destination::new(service),
            recover,
            initial_timeout,
            context_token,
            suffixes: suffixes.into_iter().collect(),
        }
    }
}

impl<S, R> tower::Service<Addr> for Client<S, R>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    S::Future: Send,
    R: Recover + Send + Clone + 'static,
    R::Backoff: Send,
{
    type Response = Option<Receiver>;
    type Error = Error;
    type Future = ProfileFuture<S, R>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, dst: Addr) -> Self::Future {
        let dst = match dst {
            Addr::Name(n) => n,
            Addr::Socket(_) => {
                self.service = self.service.clone();
                return ProfileFuture {
                    inner: ProfileFutureInner::Disabled,
                };
            }
        };

        if !self.suffixes.iter().any(|s| s.contains(dst.name())) {
            debug!("name not in profile suffixes");
            self.service = self.service.clone();
            return ProfileFuture {
                inner: ProfileFutureInner::Disabled,
            };
        }

        let service = {
            // In case the ready service holds resources, pass it into the
            // response and use a new clone for the client.
            let s = self.service.clone();
            std::mem::replace(&mut self.service, s)
        };

        let request = api::GetDestination {
            path: dst.to_string(),
            context_token: self.context_token.clone(),
            ..Default::default()
        };

        let timeout = Delay::new(Instant::now() + self.initial_timeout);

        let inner = Inner {
            service,
            request,
            recover: self.recover.clone(),
            state: State::Disconnected { backoff: None },
        };
        ProfileFuture {
            inner: ProfileFutureInner::Pending(Some(inner), timeout),
        }
    }
}

impl<S, R> Future for ProfileFuture<S, R>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    S::Future: Send,
    R: Recover + Send + 'static,
    R::Backoff: Send,
{
    type Item = Option<Receiver>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.inner {
            ProfileFutureInner::Disabled => Ok(None.into()),
            ProfileFutureInner::Pending(ref mut inner, ref mut timeout) => {
                let profile = match inner.as_mut().expect("polled after ready").poll_profile() {
                    Err(error) => {
                        trace!(%error, "failed to fetch profile");
                        return Err(error);
                    }
                    Ok(Async::NotReady) => {
                        if timeout.poll().expect("timer must not fail").is_not_ready() {
                            return Ok(Async::NotReady);
                        }

                        info!("Using default service profile after timeout");
                        profiles::Routes::default()
                    }
                    Ok(Async::Ready(profile)) => profile,
                };

                trace!("daemonizing");
                let (tx, rx) = watch::channel(profile);
                let inner = inner.take().expect("polled after ready");
                tokio::spawn(
                    Daemon { inner, tx }
                        .in_current_span()
                        .map_err(|n| match n {}),
                );

                Ok(Some(rx).into())
            }
        }
    }
}

// === impl Inner ===

impl<S, R> Inner<S, R>
where
    S: GrpcService<BoxBody>,
    R: Recover,
{
    fn poll_rx(
        rx: &mut grpc::Streaming<api::DestinationProfile, S::ResponseBody>,
    ) -> Poll<Option<profiles::Routes>, grpc::Status> {
        trace!("poll");
        let profile = try_ready!(rx.poll()).map(|proto| {
            debug!("profile received: {:?}", proto);
            let retry_budget = proto.retry_budget.and_then(convert_retry_budget);
            let routes = proto
                .routes
                .into_iter()
                .filter_map(move |orig| convert_route(orig, retry_budget.as_ref()))
                .collect();
            let dst_overrides = proto
                .dst_overrides
                .into_iter()
                .filter_map(convert_dst_override)
                .collect();
            profiles::Routes {
                routes,
                dst_overrides,
            }
        });
        Ok(profile.into())
    }

    fn poll_profile(&mut self) -> Poll<profiles::Routes, Error> {
        let span = info_span!("poll_profile");
        let _enter = span.enter();
        loop {
            self.state = match self.state {
                State::Disconnected { ref mut backoff } => {
                    trace!("disconnected");
                    try_ready!(self.service.poll_ready().map_err(Into::<Error>::into));
                    let future = self
                        .service
                        .get_profile(grpc::Request::new(self.request.clone()));
                    State::Waiting {
                        future,
                        backoff: backoff.take(),
                    }
                }
                State::Waiting {
                    ref mut future,
                    ref mut backoff,
                } => {
                    trace!("waiting");
                    match future.poll() {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(rsp)) => State::Streaming(rsp.into_inner()),
                        Err(e) => {
                            let error = e.into();
                            warn!(%error, "Could not fetch profile");
                            let new_backoff = self.recover.recover(error)?;
                            State::Disconnected {
                                backoff: Some(backoff.take().unwrap_or(new_backoff)),
                            }
                        }
                    }
                }
                State::Streaming(ref mut s) => {
                    trace!("streaming");
                    let status = match Self::poll_rx(s) {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(Some(profile))) => return Ok(profile.into()),
                        Ok(Async::Ready(None)) => grpc::Status::new(grpc::Code::Ok, ""),
                        Err(status) => status,
                    };
                    trace!(?status);
                    let backoff = self.recover.recover(status.into())?;
                    State::Backoff(Some(backoff))
                }
                State::Backoff(ref mut backoff) => {
                    trace!("backoff");
                    let backoff = match backoff.as_mut().unwrap().poll().map_err(Into::into)? {
                        Async::NotReady => return Ok(Async::NotReady),
                        Async::Ready(Some(())) => backoff.take(),
                        Async::Ready(None) => None,
                    };
                    State::Disconnected { backoff }
                }
            };
        }
    }
}

// === impl Daemon ===

impl<S, R> Future for Daemon<S, R>
where
    S: GrpcService<BoxBody>,
    R: Recover,
{
    type Item = ();
    type Error = Never;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let span = info_span!("daemon");
        let _enter = span.enter();
        trace!("poll");
        loop {
            match self.tx.poll_close() {
                Ok(Async::NotReady) => {}
                Ok(Async::Ready(())) | Err(()) => {
                    trace!("profile observation dropped");
                    return Ok(().into());
                }
            }

            let profile = match self.inner.poll_profile() {
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Ok(Async::Ready(profile)) => profile,
                Err(error) => {
                    error!(%error, "profile client died");
                    return Ok(().into());
                }
            };

            trace!(?profile, "publishing");
            if self.tx.broadcast(profile).is_err() {
                trace!("failed to publish profile");
                return Ok(().into());
            }
        }
    }
}

fn convert_route(
    orig: api::Route,
    retry_budget: Option<&Arc<Budget>>,
) -> Option<(profiles::RequestMatch, profiles::Route)> {
    let req_match = orig.condition.and_then(convert_req_match)?;
    let rsp_classes = orig
        .response_classes
        .into_iter()
        .filter_map(convert_rsp_class)
        .collect();
    let mut route = profiles::Route::new(orig.metrics_labels.into_iter(), rsp_classes);
    if orig.is_retryable {
        set_route_retry(&mut route, retry_budget);
    }
    if let Some(timeout) = orig.timeout {
        set_route_timeout(&mut route, timeout.into());
    }
    Some((req_match, route))
}

fn convert_dst_override(orig: api::WeightedDst) -> Option<profiles::WeightedAddr> {
    if orig.weight == 0 {
        return None;
    }
    NameAddr::from_str(orig.authority.as_str())
        .ok()
        .map(|addr| profiles::WeightedAddr {
            addr,
            weight: orig.weight,
        })
}

fn set_route_retry(route: &mut profiles::Route, retry_budget: Option<&Arc<Budget>>) {
    let budget = match retry_budget {
        Some(budget) => budget.clone(),
        None => {
            warn!("retry_budget is missing: {:?}", route);
            return;
        }
    };

    route.set_retries(budget);
}

fn set_route_timeout(route: &mut profiles::Route, timeout: Result<Duration, Duration>) {
    match timeout {
        Ok(dur) => {
            route.set_timeout(dur);
        }
        Err(_) => {
            warn!("route timeout is negative: {:?}", route);
        }
    }
}

fn convert_req_match(orig: api::RequestMatch) -> Option<profiles::RequestMatch> {
    let m = match orig.r#match? {
        api::request_match::Match::All(ms) => {
            let ms = ms.matches.into_iter().filter_map(convert_req_match);
            profiles::RequestMatch::All(ms.collect())
        }
        api::request_match::Match::Any(ms) => {
            let ms = ms.matches.into_iter().filter_map(convert_req_match);
            profiles::RequestMatch::Any(ms.collect())
        }
        api::request_match::Match::Not(m) => {
            let m = convert_req_match(*m)?;
            profiles::RequestMatch::Not(Box::new(m))
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
            profiles::RequestMatch::Path(re)
        }
        api::request_match::Match::Method(mm) => {
            let m = mm.r#type.and_then(|m| m.try_as_http().ok())?;
            profiles::RequestMatch::Method(m)
        }
    };

    Some(m)
}

fn convert_rsp_class(orig: api::ResponseClass) -> Option<profiles::ResponseClass> {
    let c = orig.condition.and_then(convert_rsp_match)?;
    Some(profiles::ResponseClass::new(orig.is_failure, c))
}

fn convert_rsp_match(orig: api::ResponseMatch) -> Option<profiles::ResponseMatch> {
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
            profiles::ResponseMatch::All(ms)
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
            profiles::ResponseMatch::Any(ms)
        }
        api::response_match::Match::Not(m) => {
            let m = convert_rsp_match(*m)?;
            profiles::ResponseMatch::Not(Box::new(m))
        }
        api::response_match::Match::Status(range) => {
            let min = http::StatusCode::from_u16(range.min as u16).ok()?;
            let max = http::StatusCode::from_u16(range.max as u16).ok()?;
            profiles::ResponseMatch::Status { min, max }
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
    if retry_ratio > 1000.0 || retry_ratio < 0.0 {
        warn!("retry_budget retry_ratio invalid: {:?}", retry_ratio);
        return None;
    }
    let ttl = match orig.ttl {
        Some(pb_dur) => match pb_dur.into() {
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
        },
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
        ) -> bool {
            let proto = api::RetryBudget {
                min_retries_per_second,
                retry_ratio,
                ttl: Some(prost_types::Duration {
                    seconds,
                    nanos,
                }),
            };
            convert_retry_budget(proto);
            // simply not panicking is good enough
            true
        }
    }
}
