use futures::{future, Poll, Stream};
use futures_mpsc_lossy;
use http::HeaderMap;
use indexmap::IndexMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use tower_grpc::{self as grpc, Response};

use api::tap::{server, ObserveRequest, TapEvent};
use convert::*;
use tap::{event, Event, Tap, Taps};

#[derive(Clone, Debug)]
pub struct Observe {
    next_id: Arc<AtomicUsize>,
    taps: Arc<Mutex<Taps>>,
    tap_capacity: usize,
}

pub struct TapEvents {
    rx: futures_mpsc_lossy::Receiver<Event>,
    remaining: usize,
    current: IndexMap<usize, event::Request>,
    tap_id: usize,
    taps: Arc<Mutex<Taps>>,
}

impl Observe {
    pub fn new(tap_capacity: usize) -> (Arc<Mutex<Taps>>, Observe) {
        let taps = Arc::new(Mutex::new(Taps::default()));

        let observe = Observe {
            next_id: Arc::new(AtomicUsize::new(0)),
            tap_capacity,
            taps: taps.clone(),
        };

        (taps, observe)
    }
}

impl server::Tap for Observe {
    type ObserveStream = TapEvents;
    type ObserveFuture = future::FutureResult<Response<Self::ObserveStream>, grpc::Error>;

    fn observe(&mut self, req: grpc::Request<ObserveRequest>) -> Self::ObserveFuture {
        if self.next_id.load(Ordering::Acquire) == ::std::usize::MAX {
            return future::err(grpc::Error::Grpc(
                grpc::Status::with_code(grpc::Code::Internal),
                HeaderMap::new(),
            ));
        }

        let req = req.into_inner();
        let (tap, rx) = match req.match_
            .and_then(|m| Tap::new(&m, self.tap_capacity).ok())
        {
            Some(m) => m,
            None => {
                return future::err(grpc::Error::Grpc(
                    grpc::Status::with_code(grpc::Code::InvalidArgument),
                    HeaderMap::new(),
                ));
            }
        };

        let tap_id = match self.taps.lock() {
            Ok(mut taps) => {
                let tap_id = self.next_id.fetch_add(1, Ordering::AcqRel);
                let _ = (*taps).insert(tap_id, tap);
                tap_id
            }
            Err(_) => {
                return future::err(grpc::Error::Grpc(
                    grpc::Status::with_code(grpc::Code::Internal),
                    HeaderMap::new(),
                ));
            }
        };

        let events = TapEvents {
            rx,
            tap_id,
            current: IndexMap::default(),
            remaining: req.limit as usize,
            taps: self.taps.clone(),
        };

        future::ok(Response::new(events))
    }
}

impl Stream for TapEvents {
    type Item = TapEvent;
    type Error = grpc::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            if self.remaining == 0 && self.current.is_empty() {
                trace!("tap completed");
                return Ok(None.into());
            }

            let poll: Poll<Option<Event>, Self::Error> =
                self.rx.poll().or_else(|_| Ok(None.into()));

            trace!("polling; remaining={}; current={}", self.remaining, {
                use std::fmt::Write;
                let mut s = String::new();
                write!(s, "[").unwrap();
                for id in self.current.keys() {
                    write!(s, "{},", *id).unwrap();
                }
                write!(s, "]").unwrap();
                s
            });
            match try_ready!(poll) {
                Some(ev) => {
                    match ev {
                        Event::StreamRequestOpen(ref req) => {
                            if self.remaining == 0 {
                                trace!("exhausted; ignoring req={}", req.id);
                                continue;
                            }
                            trace!("insert req={}", req.id);
                            self.remaining -= 1;
                            let _ = self.current.insert(req.id, req.clone());
                        }
                        Event::StreamRequestFail(ref req, _) => {
                            trace!("fail req={}", req.id);
                            if self.current.remove(&req.id).is_none() {
                                warn!("did not exist req={}", req.id);
                                continue;
                            }
                        }
                        Event::StreamResponseOpen(ref rsp, _) => {
                            trace!("response req={}", rsp.request.id);
                            if !self.current.contains_key(&rsp.request.id) {
                                warn!("did not exist req={}", rsp.request.id);
                                continue;
                            }
                        }
                        Event::StreamResponseFail(ref rsp, _) |
                        Event::StreamResponseEnd(ref rsp, _) => {
                            trace!("end req={}", rsp.request.id);
                            if self.current.remove(&rsp.request.id).is_none() {
                                warn!("did not exist req={}", rsp.request.id);
                                continue;
                            }
                        }
                        ev => {
                            trace!("ignoring event: {:?}", ev);
                            continue
                        }
                    }

                    trace!("emitting tap event: {:?}", ev);
                    if let Ok(te) = TapEvent::try_from(&ev) {
                        trace!("emitted tap event");
                        // TODO Do limit checks here.
                        return Ok(Some(te).into());
                    }
                }
                None => {
                    return Ok(None.into());
                }
            }
        }
    }
}

impl Drop for TapEvents {
    fn drop(&mut self) {
        if let Ok(mut taps) = self.taps.lock() {
            let _ = (*taps).remove(self.tap_id);
        }
    }
}
