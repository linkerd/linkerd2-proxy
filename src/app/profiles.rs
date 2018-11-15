use futures::{Async, Future, Poll, Stream};
use http;
use regex::Regex;
use std::fmt;
use std::time::Duration;
use tokio_timer::{clock, Delay};
use tower_grpc::{self as grpc, Body, BoxBody};
use tower_http::HttpService;

use api::destination as api;

use proxy::http::profiles;
use NameAddr;

#[derive(Clone, Debug)]
pub struct Client<T> {
    service: Option<T>,
    backoff: Duration,
}

pub struct Rx<T>
where
    T: HttpService<BoxBody>,
    T::ResponseBody: Body,
{
    dst: String,
    backoff: Duration,
    service: Option<T>,
    state: State<T>,
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
    T: HttpService<BoxBody> + Clone,
    T::ResponseBody: Body,
    T::Error: fmt::Debug,
{
    pub fn new(service: Option<T>, backoff: Duration) -> Self {
        Self {
            service,
            backoff,
        }
    }
}

impl<T> profiles::GetRoutes for Client<T>
where
    T: HttpService<BoxBody> + Clone,
    T::ResponseBody: Body,
    T::Error: fmt::Debug,
{
    type Stream = Rx<T>;

    fn get_routes(&self, dst: &NameAddr) -> Option<Self::Stream> {
        Some(Rx {
            dst: format!("{}", dst),
            state: State::Disconnected,
            service: self.service.clone(),
            backoff: self.backoff,
        })
    }
}

// === impl Rx ===

impl<T> Stream for Rx<T>
where
    T: HttpService<BoxBody> + Clone,
    T::ResponseBody: Body,
    T::Error: fmt::Debug,
{
    type Item = Vec<(profiles::RequestMatch, profiles::Route)>;
    type Error = profiles::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let service = match self.service {
            Some(ref s) => s,
            None => return Ok(Async::Ready(Some(Vec::new()))),
        };

        loop {
            self.state = match self.state {
                State::Disconnected => {
                    let mut client = api::client::Destination::new(service.clone());
                    let req = api::GetDestination {
                        scheme: "k8s".to_owned(),
                        path: self.dst.clone(),
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
                State::Streaming(ref mut s) => match s.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(Some(profile))) => {
                        debug!("profile received: {:?}", profile);
                        let rs = profile.routes.into_iter().filter_map(convert_route);
                        return Ok(Async::Ready(Some(rs.collect())));
                    }
                    Ok(Async::Ready(None)) => {
                        debug!("profile stream ended");
                        State::Backoff(Delay::new(clock::now() + self.backoff))
                    }
                    Err(e) => {
                        warn!("profile stream failed: {:?}", e);
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

fn convert_route(orig: api::Route) -> Option<(profiles::RequestMatch, profiles::Route)> {
    let req_match = orig.condition.and_then(convert_req_match)?;
    let rsp_classes = orig
        .response_classes
        .into_iter()
        .filter_map(convert_rsp_class)
        .collect();
    let route = profiles::Route::new(orig.metrics_labels.into_iter(), rsp_classes);
    Some((req_match, route))
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
            let re = Regex::new(&regex).ok()?;
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
