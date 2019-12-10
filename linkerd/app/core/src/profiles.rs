use crate::dns;
use crate::proxy::http::{profiles, retry::Budget};
use futures::{Async, Future, Poll, Stream};
use http;
use linkerd2_addr::NameAddr;
use linkerd2_error::Never;
use linkerd2_proxy_api::destination as api;
use regex::Regex;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, watch};
use tokio_timer::{clock, Delay};
use tower_grpc::{self as grpc, generic::client::GrpcService, Body, BoxBody};
use tracing::{debug, error, trace, warn};
use tracing_futures::Instrument;

#[derive(Clone, Debug)]
pub struct Client<T> {
    service: api::client::Destination<T>,
    backoff: Duration,
    context_token: String,
    suffixes: Vec<dns::Suffix>,
}

pub struct Rx {
    rx: watch::Receiver<profiles::Routes>,
    _hangup: oneshot::Sender<Never>,
}

struct Daemon<T>
where
    T: GrpcService<BoxBody>,
{
    backoff: Duration,
    service: api::client::Destination<T>,
    state: State<T>,
    tx: watch::Sender<profiles::Routes>,
    hangup: oneshot::Receiver<Never>,
    request: api::GetDestination,
}

enum State<T>
where
    T: GrpcService<BoxBody>,
{
    Disconnected,
    Backoff(Delay),
    Waiting(grpc::client::server_streaming::ResponseFuture<api::DestinationProfile, T::Future>),
    Streaming(grpc::Streaming<api::DestinationProfile, T::ResponseBody>),
}

// === impl Client ===

impl<T> Client<T>
where
    // These bounds aren't *required* here, they just help detect the problem
    // earlier (as Client::new), instead of when trying to passing a `Client`
    // to something that wants `impl profiles::GetRoutes`.
    T: GrpcService<BoxBody> + Clone + Send + 'static,
    T::ResponseBody: Send,
    <T::ResponseBody as Body>::Data: Send,
    T::Future: Send,
{
    pub fn new(
        service: T,
        backoff: Duration,
        context_token: String,
        suffixes: impl IntoIterator<Item = dns::Suffix>,
    ) -> Self {
        Self {
            service: api::client::Destination::new(service),
            backoff,
            context_token,
            suffixes: suffixes.into_iter().collect(),
        }
    }
}

impl<T> profiles::GetRoutes for Client<T>
where
    T: GrpcService<BoxBody> + Clone + Send + 'static,
    T::ResponseBody: Send,
    <T::ResponseBody as Body>::Data: Send,
    T::Future: Send,
{
    type Stream = Rx;

    fn get_routes(&self, dst: &NameAddr) -> Option<Self::Stream> {
        if !self.suffixes.iter().any(|s| s.contains(dst.name())) {
            debug!("name not in profile suffixes");
            return None;
        }
        debug!("watching routes");

        // This oneshot allows the daemon to be notified when the Self::Stream
        // is dropped.
        let (hangup_tx, hangup_rx) = oneshot::channel();
        let (tx, rx) = watch::channel(profiles::Routes::default());
        let daemon = Daemon {
            tx,
            hangup: hangup_rx,
            state: State::Disconnected,
            service: self.service.clone(),
            backoff: self.backoff,
            request: api::GetDestination {
                path: format!("{}", dst),
                context_token: self.context_token.clone(),
                ..Default::default()
            },
        };

        tokio::spawn(daemon.in_current_span().map_err(|e| match e {}));
        Some(Rx {
            rx,
            _hangup: hangup_tx,
        })
    }
}

// === impl Rx ===

impl Stream for Rx {
    type Item = profiles::Routes;
    type Error = Never;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.rx.poll().or_else(|_| Ok(None.into()))
    }
}

// === impl Daemon ===

enum StreamState {
    SendLost,
    RecvDone,
}

impl<T> Daemon<T>
where
    T: GrpcService<BoxBody>,
{
    fn proxy_stream(
        rx: &mut grpc::Streaming<api::DestinationProfile, T::ResponseBody>,
        tx: &mut watch::Sender<profiles::Routes>,
        hangup: &mut oneshot::Receiver<Never>,
    ) -> Async<StreamState> {
        loop {
            match rx.poll() {
                Ok(Async::NotReady) => match hangup.poll() {
                    Ok(Async::Ready(never)) => match never {}, // unreachable!
                    Ok(Async::NotReady) => {
                        // We are now scheduled to be notified if the hangup tx
                        // is dropped.
                        return Async::NotReady;
                    }
                    Err(_) => {
                        // Hangup tx has been dropped.
                        debug!("profile stream cancelled");
                        return StreamState::SendLost.into();
                    }
                },
                Ok(Async::Ready(None)) => return StreamState::RecvDone.into(),
                Ok(Async::Ready(Some(proto))) => {
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
                    let profile = profiles::Routes {
                        routes,
                        dst_overrides,
                    };
                    if tx.broadcast(profile).is_err() {
                        return StreamState::SendLost.into();
                    }
                }
                Err(e) => {
                    warn!("profile stream failed: {:?}", e);
                    return StreamState::RecvDone.into();
                }
            }
        }
    }
}

impl<T> Future for Daemon<T>
where
    T: GrpcService<BoxBody>,
{
    type Item = ();
    type Error = Never;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.state = match self.state {
                State::Disconnected => {
                    match self.service.poll_ready() {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(())) => {}
                        Err(err) => {
                            error!("profile service unexpected error: {:?}", err,);
                            return Ok(Async::Ready(()));
                        }
                    };
                    debug!("getting profile");
                    let rspf = self
                        .service
                        .get_profile(grpc::Request::new(self.request.clone()));
                    State::Waiting(rspf)
                }
                State::Waiting(ref mut f) => match f.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(rsp)) => {
                        trace!("response received");
                        State::Streaming(rsp.into_inner())
                    }
                    Err(e) => {
                        warn!("error fetching profile: {:?}", e);
                        State::Backoff(Delay::new(clock::now() + self.backoff))
                    }
                },
                State::Streaming(ref mut s) => {
                    match Self::proxy_stream(s, &mut self.tx, &mut self.hangup) {
                        Async::NotReady => return Ok(Async::NotReady),
                        Async::Ready(StreamState::SendLost) => return Ok(().into()),
                        Async::Ready(StreamState::RecvDone) => {
                            State::Backoff(Delay::new(clock::now() + self.backoff))
                        }
                    }
                }
                State::Backoff(ref mut f) => match f.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(_) | Ok(Async::Ready(())) => State::Disconnected,
                },
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
