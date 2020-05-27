use crate::http as profiles;
use api::destination_client::DestinationClient;
use futures::{future, ready, Stream, TryStreamExt};
use http;
use http_body::Body as HttpBody;
use linkerd2_addr::{Addr, NameAddr};
use linkerd2_dns as dns;
use linkerd2_error::{Error, Recover};
use linkerd2_proxy_api::destination as api;
use pin_project::{pin_project, project};
use regex::Regex;
use std::convert::TryInto;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::{self, Delay};
use tonic::{
    self as grpc,
    body::{Body, BoxBody},
    client::GrpcService,
};
use tower::retry::budget::Budget;
use tracing::{debug, error, info, info_span, trace, warn};
use tracing_futures::Instrument;

#[derive(Clone, Debug)]
pub struct Client<S, R> {
    service: DestinationClient<S>,
    recover: R,
    initial_timeout: Duration,
    context_token: String,
    suffixes: Vec<dns::Suffix>,
}

pub type Receiver = watch::Receiver<profiles::Routes>;

type Sender = watch::Sender<profiles::Routes>;

#[derive(Clone, Debug)]
pub struct InvalidProfileAddr(Addr);

#[pin_project]
pub struct ProfileFuture<S, R>
where
    S: GrpcService<BoxBody>,
    R: Recover,
{
    #[pin]
    inner: ProfileFutureInner<S, R>,
}

#[pin_project]
enum ProfileFutureInner<S, R>
where
    S: GrpcService<BoxBody>,
    R: Recover,
{
    Invalid(Addr),
    Pending(#[pin] Option<Inner<S, R>>, #[pin] Delay),
}

struct Daemon<S, R>
where
    S: GrpcService<BoxBody>,
    R: Recover,
{
    tx: Sender,
    inner: Inner<S, R>,
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

#[pin_project]
enum State<B> {
    Disconnected {
        backoff: Option<B>,
    },
    Waiting {
        future: Pin<
            Box<
                dyn Future<
                        Output = Result<
                            tonic::Response<tonic::Streaming<api::DestinationProfile>>,
                            grpc::Status,
                        >,
                    > + Send
                    + 'static,
            >,
        >,
        backoff: Option<B>,
    },
    Streaming(#[pin] grpc::Streaming<api::DestinationProfile>),
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
    <S::ResponseBody as HttpBody>::Error:
        Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
    R: Recover,
    R::Backoff: Unpin,
{
    pub fn new(
        service: S,
        recover: R,
        initial_timeout: Duration,
        context_token: String,
        suffixes: impl IntoIterator<Item = dns::Suffix>,
    ) -> Self {
        Self {
            service: DestinationClient::new(service),
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
    <S::ResponseBody as HttpBody>::Error:
        Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
    R: Recover + Send + Clone + 'static,
    R::Backoff: Unpin + Send,
{
    type Response = Receiver;
    type Error = Error;
    type Future = ProfileFuture<S, R>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Tonic will internally drive the client service to readiness.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Addr) -> Self::Future {
        let dst = match dst {
            Addr::Name(n) => n,
            Addr::Socket(_) => {
                self.service = self.service.clone();
                return ProfileFuture {
                    inner: ProfileFutureInner::Invalid(dst),
                };
            }
        };

        if !self.suffixes.iter().any(|s| s.contains(dst.name())) {
            debug!("name not in profile suffixes");
            self.service = self.service.clone();
            return ProfileFuture {
                inner: ProfileFutureInner::Invalid(dst.into()),
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

        let timeout = time::delay_for(self.initial_timeout);

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
    <S::ResponseBody as HttpBody>::Error:
        Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
    R: Recover + Send + 'static,
    R::Backoff: Unpin,
    R::Backoff: Send,
{
    type Output = Result<Receiver, Error>;

    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        #[project]
        match self.project().inner.project() {
            ProfileFutureInner::Invalid(addr) => {
                Poll::Ready(Err(InvalidProfileAddr(addr.clone()).into()))
            }
            ProfileFutureInner::Pending(mut inner, timeout) => {
                let profile = match inner
                    .as_mut()
                    .as_pin_mut()
                    .expect("polled after ready")
                    .poll_profile(cx)
                {
                    Poll::Ready(Err(error)) => {
                        trace!(%error, "failed to fetch profile");
                        return Poll::Ready(Err(error));
                    }
                    Poll::Pending => {
                        if timeout.poll(cx).is_pending() {
                            return Poll::Pending;
                        }

                        info!("Using default service profile after timeout");
                        profiles::Routes::default()
                    }
                    Poll::Ready(Ok(profile)) => profile,
                };

                trace!("daemonizing");
                let (mut tx, rx) = watch::channel(profile);
                let inner = inner.take().expect("polled after ready");
                let daemon = async move {
                    tokio::pin!(inner);
                    loop {
                        tokio::select! {
                            _ = tx.closed() => {
                                trace!("profile observation dropped");
                                return;
                            },
                            profile = future::poll_fn(|cx|
                                inner.as_mut().poll_profile(cx)
                             ) => {
                                match profile {
                                    Err(error) => {
                                        error!(%error, "profile client died");
                                        return;
                                    }
                                    Ok(profile) => {
                                        trace!(?profile, "publishing");
                                        if tx.broadcast(profile).is_err() {
                                            trace!("failed to publish profile");
                                            return
                                        }
                                    }
                                }
                            }
                        }
                    }
                };
                tokio::spawn(daemon.in_current_span());

                Poll::Ready(Ok(rx))
            }
        }
    }
}

// === impl Inner ===

impl<S, R> Inner<S, R>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send + 'static,
    <S::ResponseBody as HttpBody>::Error:
        Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
    R: Recover,
    R::Backoff: Unpin,
{
    fn poll_rx(
        rx: Pin<&mut grpc::Streaming<api::DestinationProfile>>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<profiles::Routes, grpc::Status>>> {
        trace!("poll");
        let profile = ready!(rx.poll_next(cx)).map(|res| {
            res.map(|proto| {
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
            })
        });
        Poll::Ready(profile)
    }

    #[project]
    fn poll_profile(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<profiles::Routes, Error>> {
        let span = info_span!("poll_profile");
        let _enter = span.enter();

        loop {
            let mut this = self.as_mut().project();
            #[project]
            match this.state.as_mut().project() {
                State::Disconnected { backoff } => {
                    trace!("disconnected");
                    let mut svc = this.service.clone();
                    let req = this.request.clone();
                    let future =
                        Box::pin(async move { svc.get_profile(grpc::Request::new(req)).await });
                    let backoff = backoff.take();
                    this.state.as_mut().set(State::Waiting { future, backoff });
                }
                State::Waiting { future, backoff } => {
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
                State::Streaming(s) => {
                    trace!("streaming");
                    let status = match ready!(Self::poll_rx(s, cx)) {
                        Some(Ok(profile)) => return Poll::Ready(Ok(profile.into())),
                        None => grpc::Status::new(grpc::Code::Ok, ""),
                        Some(Err(status)) => status,
                    };
                    trace!(?status);
                    let backoff = this.recover.recover(status.into())?;
                    this.state.set(State::Backoff(Some(backoff)));
                }
                State::Backoff(ref mut backoff) => {
                    trace!("backoff");
                    let backoff = match ready!(backoff.as_mut().unwrap().try_poll_next_unpin(cx)) {
                        Some(e) => {
                            e.map_err(Into::into)?;
                            backoff.take()
                        }
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
        set_route_timeout(&mut route, timeout.try_into());
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
            let m = mm.r#type.and_then(|m| (&m).try_into().ok())?;
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
        Some(pb_dur) => match pb_dur.try_into() {
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

impl InvalidProfileAddr {
    pub fn addr(&self) -> &Addr {
        &self.0
    }
}

impl std::fmt::Display for InvalidProfileAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid profile addr: {}", self.0)
    }
}

impl std::error::Error for InvalidProfileAddr {}
