use bytes::IntoBuf;
use futures::sync::mpsc;
use futures::{Async, AsyncSink, Future, Poll, Sink, Stream};
use http;
use regex::Regex;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::executor::{DefaultExecutor, Executor};
use tokio_timer::{clock, Delay};
use tower_grpc::{self as grpc, Body, BoxBody};
use tower_http::HttpService;
use tower_retry::budget::Budget;

use api::destination as api;
use never::Never;

use proxy::http::profiles;
use NameAddr;

#[derive(Clone, Debug)]
pub struct Client<T> {
    service: Option<T>,
    backoff: Duration,
    proxy_id: String,
}

pub struct Rx(mpsc::Receiver<profiles::Routes>);

struct Daemon<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
{
    dst: String,
    backoff: Duration,
    service: Option<T>,
    state: State<T>,
    tx: mpsc::Sender<profiles::Routes>,
    proxy_id: String,
}

enum State<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
{
    Disconnected,
    Backoff(Delay),
    Waiting(grpc::client::server_streaming::ResponseFuture<api::DestinationProfile, T::Future>),
    Streaming(grpc::Streaming<api::DestinationProfile, T::ResponseBody>),
}

// === impl Client ===

impl<T> Client<T>
where
    T: HttpService<BoxBody> + Clone + Send + 'static,
    T::ResponseBody: Body + Send + 'static,
    <T::ResponseBody as Body>::Data: Send + 'static,
    <<T::ResponseBody as Body>::Data as IntoBuf>::Buf: Send + 'static,
    T::Error: fmt::Debug,
{
    pub fn new(service: Option<T>, backoff: Duration, proxy_id: String) -> Self {
        Self { service, backoff, proxy_id }
    }
}

impl<T> profiles::GetRoutes for Client<T>
where
    T: HttpService<BoxBody> + Clone + Send + 'static,
    T::Future: Send + 'static,
    T::ResponseBody: Body + Send + 'static,
    <T::ResponseBody as Body>::Data: Send + 'static,
    <<T::ResponseBody as Body>::Data as IntoBuf>::Buf: Send + 'static,
    T::Error: fmt::Debug,
 {
    type Stream = Rx;

    fn get_routes(&self, dst: &NameAddr) -> Option<Self::Stream> {
        let (tx, rx) = mpsc::channel(1);

        let daemon = Daemon {
            tx,
            dst: format!("{}", dst),
            state: State::Disconnected,
            service: self.service.clone(),
            backoff: self.backoff,
            proxy_id: self.proxy_id.clone(),
        };
        let spawn = DefaultExecutor::current().spawn(Box::new(daemon.map_err(|_| ())));

        spawn.ok().map(|_| Rx(rx))
    }
}

// === impl Rx ===

impl Stream for Rx {
    type Item = profiles::Routes;
    type Error = Never;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.0.poll().or_else(|_| Ok(None.into()))
    }
}

// === impl Daemon ===

enum StreamState {
    SendLost,
    RecvDone,
}

impl<T> Daemon<T>
where
    T: HttpService<BoxBody> + Clone,
    T::ResponseBody: Body,
    T::Error: fmt::Debug,
{
    fn proxy_stream(
        rx: &mut grpc::Streaming<api::DestinationProfile, T::ResponseBody>,
        tx: &mut mpsc::Sender<profiles::Routes>,
    ) -> Async<StreamState> {
        loop {
            match tx.poll_ready() {
                Ok(Async::NotReady) => return Async::NotReady,
                Ok(Async::Ready(())) => {}
                Err(_) => return StreamState::SendLost.into(),
            }

            match rx.poll() {
                Ok(Async::NotReady) => return Async::NotReady,
                Ok(Async::Ready(None)) => return StreamState::RecvDone.into(),
                Ok(Async::Ready(Some(profile))) => {
                    debug!("profile received: {:?}", profile);
                    let retry_budget = profile.retry_budget.and_then(convert_retry_budget);
                    let routes = profile
                        .routes
                        .into_iter()
                        .filter_map(move |orig| {
                            convert_route(orig, retry_budget.as_ref())
                        });
                    match tx.start_send(routes.collect()) {
                        Ok(AsyncSink::Ready) => {} // continue
                        Ok(AsyncSink::NotReady(_)) => {
                            info!("dropping profile update due to a full buffer");
                            // This must have been because another task stole
                            // our tx slot? It seems pretty unlikely, but possible?
                            return Async::NotReady;
                        }
                        Err(_) => {
                            return StreamState::SendLost.into();
                        }
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
    T: HttpService<BoxBody> + Clone,
    T::ResponseBody: Body,
    T::Error: fmt::Debug,
{
    type Item = ();
    type Error = Never;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.state = match self.state {
                State::Disconnected => {
                    let mut client = match self.service {
                        Some(ref svc) => api::client::Destination::new(svc.clone()),
                        None => return Ok(Async::Ready(())),
                    };

                    let req = api::GetDestination {
                        scheme: "k8s".to_owned(),
                        path: self.dst.clone(),
                        proxy_id: self.proxy_id.clone(),
                    };
                    debug!("disconnected; getting profile: {:?}", req);
                    let rspf = client.get_profile(grpc::Request::new(req));
                    State::Waiting(rspf)
                }
                State::Waiting(ref mut f) => match f.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(rsp)) => {
                        debug!("response received");
                        State::Streaming(rsp.into_inner())
                    }
                    Err(e) => {
                        warn!("error fetching profile for {}: {:?}", self.dst, e);
                        State::Backoff(Delay::new(clock::now() + self.backoff))
                    }
                },
                State::Streaming(ref mut s) => match Self::proxy_stream(s, &mut self.tx) {
                    Async::NotReady => return Ok(Async::NotReady),
                    Async::Ready(StreamState::SendLost) => return Ok(().into()),
                    Async::Ready(StreamState::RecvDone) => {
                        State::Backoff(Delay::new(clock::now() + self.backoff))
                    }
                },
                State::Backoff(ref mut f) => match f.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(_) | Ok(Async::Ready(())) => State::Disconnected,
                },
            };
        }
    }
}

fn convert_route(orig: api::Route, retry_budget: Option<&Arc<Budget>>) -> Option<(profiles::RequestMatch, profiles::Route)> {
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

fn set_route_retry(route: &mut profiles::Route, retry_budget: Option<&Arc<Budget>>) {
    let budget = match retry_budget {
        Some(budget) => budget.clone(),
        None => {
            warn!("retry_budget is missing: {:?}", route);
            return;
        },
    };

    route.set_retries(budget);
}

fn set_route_timeout(route: &mut profiles::Route, timeout: Result<Duration, Duration>) {
    match timeout {
        Ok(dur) => {
            route.set_timeout(dur);
        },
        Err(_) => {
            warn!("route timeout is negative: {:?}", route);
        },
    }
}

fn convert_req_match(orig: api::RequestMatch) -> Option<profiles::RequestMatch> {
    let m = match orig.match_? {
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
            let m = mm.type_.and_then(|m| m.try_as_http().ok())?;
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
    let m = match orig.match_? {
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
        warn!("retry_budget min_retries_per_second overflow: {:?}", orig.min_retries_per_second);
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
            },
            Err(negative) => {
                warn!("retry_budget ttl negative: {:?}", negative);
                return None;
            },
        },
        None => {
            warn!("retry_budget ttl missing");
            return None;
        },
    };

    Some(Arc::new(Budget::new(ttl, min_retries, retry_ratio)))
}

#[cfg(test)]
mod tests {
    use quickcheck::*;
    use super::*;

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
                ttl: Some(::prost_types::Duration {
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

