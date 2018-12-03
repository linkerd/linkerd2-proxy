use bytes::Buf;
use futures::sync::{mpsc, oneshot};
use futures::{future, Async, Future, Poll, Stream};
use http::HeaderMap;
use hyper::body::Payload;
use never::Never;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;
use tokio_timer::clock;
use tower_grpc::{self as grpc, Response};

use api::{http_types, pb_duration, tap as api};

use super::match_::Match;
use proxy::http::HasH2Reason;
use tap::{iface, Inspect};

#[derive(Clone, Debug)]
pub struct Server<T> {
    subscribe: T,
    base_id: Arc<AtomicUsize>,
}

#[derive(Debug)]
pub struct ResponseFuture<F> {
    subscribe: F,
    dispatch: Option<Dispatch>,
    events_rx: Option<mpsc::Receiver<api::TapEvent>>,
}

#[derive(Debug)]
pub struct ResponseStream {
    dispatch: Option<Dispatch>,
    events_rx: mpsc::Receiver<api::TapEvent>,
}

#[derive(Debug)]
struct Dispatch {
    base_id: u32,
    count: usize,
    limit: usize,
    taps_rx: mpsc::Receiver<oneshot::Sender<TapTx>>,
    events_tx: mpsc::Sender<api::TapEvent>,
    match_handle: Arc<Match>,
}

#[derive(Clone, Debug)]
struct TapTx {
    id: api::tap_event::http::StreamId,
    tx: mpsc::Sender<api::TapEvent>,
}

#[derive(Clone, Debug)]
pub struct Tap {
    match_: Weak<Match>,
    taps_tx: mpsc::Sender<oneshot::Sender<TapTx>>,
}

#[derive(Debug)]
pub struct TapFuture(FutState);

#[derive(Debug)]
enum FutState {
    Init {
        request_init_at: Instant,
        taps_tx: mpsc::Sender<oneshot::Sender<TapTx>>,
    },
    Pending {
        request_init_at: Instant,
        rx: oneshot::Receiver<TapTx>,
    },
}

#[derive(Debug)]
pub struct TapRequest {
    request_init_at: Instant,
    tap: TapTx,
}

#[derive(Debug)]
pub struct TapResponse {
    base_event: api::TapEvent,
    request_init_at: Instant,
    tap: TapTx,
}

#[derive(Debug)]
pub struct TapRequestPayload {
    base_event: api::TapEvent,
    tap: TapTx,
}

#[derive(Debug)]
pub struct TapResponsePayload {
    base_event: api::TapEvent,
    request_init_at: Instant,
    response_init_at: Instant,
    response_bytes: usize,
    tap: TapTx,
    // Response-headers may include grpc-status when there is no response body.
    grpc_status: Option<u32>,
}

// === impl Server ===

impl<T: iface::Subscribe<Tap>> Server<T> {
    pub(in tap) fn new(subscribe: T) -> Self {
        let base_id = Arc::new(0.into());
        Self { base_id, subscribe }
    }

    fn invalid_arg(event: http::header::HeaderValue) -> grpc::Error {
        let status = grpc::Status::with_code(grpc::Code::InvalidArgument);
        let mut headers = HeaderMap::new();
        headers.insert("grpc-message", event);
        grpc::Error::Grpc(status, headers)
    }
}

impl<T> api::server::Tap for Server<T>
where
    T: iface::Subscribe<Tap> + Clone,
{
    type ObserveStream = ResponseStream;
    type ObserveFuture = future::Either<
        future::FutureResult<Response<Self::ObserveStream>, grpc::Error>,
        ResponseFuture<T::Future>,
    >;

    fn observe(&mut self, req: grpc::Request<api::ObserveRequest>) -> Self::ObserveFuture {
        let req = req.into_inner();

        let limit = req.limit as usize;
        if limit == 0 {
            let v = http::header::HeaderValue::from_static("limit must be positive");
            return future::Either::A(future::err(Self::invalid_arg(v)));
        };
        trace!("tap: limit={}", limit);

        // Read the match logic into a type we can use to evaluate against
        // requests. This match will be shared (weakly) by all registered
        // services to match requests. The response stream strongly holds the
        // match until the response is complete. This way, services never
        // evaluate matches for taps that have been completed or canceled.
        let match_handle = match Match::try_new(req.match_) {
            Ok(m) => Arc::new(m),
            Err(e) => {
                warn!("invalid tap request: {} ", e);
                let v = format!("{}", e)
                    .parse()
                    .unwrap_or_else(|_| http::header::HeaderValue::from_static("invalid message"));
                return future::Either::A(future::err(Self::invalid_arg(v)));
            }
        };

        // Wrapping is okay. This is realy just to disambiguate events within a
        // single tap session (i.e. that may consist of several tap requests).
        let base_id = self.base_id.fetch_add(1, Ordering::AcqRel) as u32;
        debug!("tap; id={}; match={:?}", base_id, match_handle);

        // The taps channel is used by services to acquire a `TapTx` for
        // `Dispatch`, i.e. ensuring that no more than the requested number of
        // taps are executed.
        //
        // The read side of this channel (held by `dispatch`) is dropped by the
        // `ResponseStream` once the `limit` has been reached. This is dropped
        // with the strong reference to `match_` so that services can determine
        // when a Tap should be dropped.
        //
        // The response stream continues to process events for open streams
        // until all streams have been completed.
        let (taps_tx, taps_rx) = mpsc::channel(super::super::TAP_CAPACITY);

        let tap = Tap::new(Arc::downgrade(&match_handle), taps_tx);
        let subscribe = self.subscribe.subscribe(tap);

        // The events channel is used to emit tap events to the response stream.
        //
        // At most `limit` copies of `events_tx` are dispatched to `taps_rx`
        // requests. Each tapped request's sender is dropped when the response
        // completes, so the event stream closes gracefully when all tapped
        // requests are completed without additional coordination.
        let (events_tx, events_rx) =
            mpsc::channel(super::super::PER_RESPONSE_EVENT_BUFFER_CAPACITY);

        // Reads up to `limit` requests from from `taps_rx` and satisfies them
        // with a cpoy of `events_tx`.
        let dispatch = Dispatch {
            base_id,
            count: 0,
            limit,
            taps_rx,
            events_tx,
            match_handle,
        };

        future::Either::B(ResponseFuture {
            subscribe,
            dispatch: Some(dispatch),
            events_rx: Some(events_rx),
        })
    }
}

impl<F: Future<Item = ()>> Future for ResponseFuture<F> {
    type Item = Response<ResponseStream>;
    type Error = grpc::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Ensure that tap registers successfully.
        match self.subscribe.poll() {
            Ok(Async::NotReady) => return Ok(Async::NotReady),
            Ok(Async::Ready(())) => {}
            Err(_) => {
                let status = grpc::Status::with_code(grpc::Code::ResourceExhausted);
                let mut headers = HeaderMap::new();
                headers.insert("grpc-message", "Too many active taps".parse().unwrap());
                return Err(grpc::Error::Grpc(status, headers));
            }
        }

        let rsp = ResponseStream {
            dispatch: self.dispatch.take(),
            events_rx: self.events_rx.take().expect("events_rx must be set"),
        };

        Ok(Response::new(rsp).into())
    }
}

// === impl ResponseStream ===

impl Stream for ResponseStream {
    type Item = api::TapEvent;
    type Error = grpc::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        // Drop the dispatch future once it completes so that services do not do
        // any more matching against this tap.
        //
        // Furthermore, this drops the event sender so that `events_rx` closes
        // gracefully when all open taps are complete.
        self.dispatch = self.dispatch.take().and_then(|mut d| match d.poll() {
            Ok(Async::NotReady) => Some(d),
            Ok(Async::Ready(())) | Err(_) => None,
        });

        // Read events from taps. The receiver can't actually error, but we need
        // to satisfy the type signature, so we coerce errors into EOS.
        self.events_rx.poll().or_else(|_| Ok(None.into()))
    }
}

// === impl Dispatch ===

impl Future for Dispatch {
    type Item = ();
    type Error = ();

    /// Read tap requests, assign each an ID and send it back to the requesting
    /// service with an event sender so that it may emittap events.
    ///
    /// Becomes ready when the limit has been reached.
    fn poll(&mut self) -> Poll<(), Self::Error> {
        while let Some(tx) = try_ready!(self.taps_rx.poll().map_err(|_| ())) {
            debug_assert!(self.count < self.limit - 1);

            self.count += 1;
            let tap = TapTx {
                tx: self.events_tx.clone(),
                id: api::tap_event::http::StreamId {
                    base: self.base_id,
                    stream: self.count as u64,
                },
            };
            if tx.send(tap).is_err() {
                // If the tap isn't sent, then restore the count.
                self.count -= 1;
            }

            if self.count == self.limit - 1 {
                return Ok(Async::Ready(()));
            }
        }

        Ok(Async::Ready(()))
    }
}

// === impl Tap ===

impl Tap {
    fn new(match_: Weak<Match>, taps_tx: mpsc::Sender<oneshot::Sender<TapTx>>) -> Self {
        Self { match_, taps_tx }
    }
}

impl iface::Tap for Tap {
    type TapRequest = TapRequest;
    type TapRequestPayload = TapRequestPayload;
    type TapResponse = TapResponse;
    type TapResponsePayload = TapResponsePayload;
    type Future = TapFuture;

    fn can_tap_more(&self) -> bool {
        self.match_.upgrade().is_some()
    }

    fn should_tap<B: Payload, I: Inspect>(&self, req: &http::Request<B>, inspect: &I) -> bool {
        self.match_
            .upgrade()
            .map(|m| m.matches(req, inspect))
            .unwrap_or(false)
    }

    fn tap(&mut self) -> Self::Future {
        TapFuture(FutState::Init {
            request_init_at: clock::now(),
            taps_tx: self.taps_tx.clone(),
        })
    }
}

// === impl TapFuture ===

impl Future for TapFuture {
    type Item = Option<TapRequest>;
    type Error = Never;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.0 = match self.0 {
                FutState::Init {
                    ref request_init_at,
                    ref mut taps_tx,
                } => {
                    match taps_tx.poll_ready() {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(())) => {}
                        Err(_) => return Ok(Async::Ready(None)),
                    }
                    let (tx, rx) = oneshot::channel();

                    // If this fails, polling `rx` will fail below.
                    let _ = taps_tx.try_send(tx);

                    FutState::Pending {
                        request_init_at: *request_init_at,
                        rx,
                    }
                }
                FutState::Pending {
                    ref request_init_at,
                    ref mut rx,
                } => {
                    return match rx.poll() {
                        Ok(Async::NotReady) => Ok(Async::NotReady),
                        Ok(Async::Ready(tap)) => {
                            let t = TapRequest {
                                request_init_at: *request_init_at,
                                tap,
                            };
                            Ok(Some(t).into())
                        }
                        Err(_) => Ok(None.into()),
                    };
                }
            }
        }
    }
}

// === impl TapRequest ===

impl iface::TapRequest for TapRequest {
    type TapPayload = TapRequestPayload;
    type TapResponse = TapResponse;
    type TapResponsePayload = TapResponsePayload;

    fn open<B: Payload, I: Inspect>(
        mut self,
        req: &http::Request<B>,
        inspect: &I,
    ) -> (TapRequestPayload, TapResponse) {
        let base_event = base_event(req, inspect);

        let init = api::tap_event::http::RequestInit {
            id: Some(self.tap.id.clone()),
            method: Some(req.method().into()),
            scheme: req.uri().scheme_part().map(http_types::Scheme::from),
            authority: inspect.authority(req).unwrap_or_default(),
            path: req.uri().path().into(),
        };
;
        let event = api::TapEvent {
            event: Some(api::tap_event::Event::Http(api::tap_event::Http {
                event: Some(api::tap_event::http::Event::RequestInit(init)),
            })),
            ..base_event.clone()
        };
        let _ = self.tap.tx.try_send(event);

        let req = TapRequestPayload {
            tap: self.tap.clone(),
            base_event: base_event.clone(),
        };
        let rsp = TapResponse {
            tap: self.tap,
            base_event,
            request_init_at: self.request_init_at,
        };
        (req, rsp)
    }
}

// === impl TapResponse ===

impl iface::TapResponse for TapResponse {
    type TapPayload = TapResponsePayload;

    fn tap<B: Payload>(mut self, rsp: &http::Response<B>) -> TapResponsePayload {
        let response_init_at = clock::now();
        let init = api::tap_event::http::Event::ResponseInit(api::tap_event::http::ResponseInit {
            id: Some(self.tap.id.clone()),
            since_request_init: Some(pb_duration(response_init_at - self.request_init_at)),
            http_status: rsp.status().as_u16().into(),
        });

        let event = api::TapEvent {
            event: Some(api::tap_event::Event::Http(api::tap_event::Http {
                event: Some(init),
            })),
            ..self.base_event.clone()
        };
        let _ = self.tap.tx.try_send(event);

        TapResponsePayload {
            base_event: self.base_event,
            request_init_at: self.request_init_at,
            response_init_at,
            response_bytes: 0,
            tap: self.tap,
            grpc_status: rsp
                .headers()
                .get("grpc-status")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u32>().ok()),
        }
    }

    fn fail<E: HasH2Reason>(mut self, err: &E) {
        let response_end_at = clock::now();
        let reason = err.h2_reason();
        let end = api::tap_event::http::Event::ResponseEnd(api::tap_event::http::ResponseEnd {
            id: Some(self.tap.id.clone()),
            since_request_init: Some(pb_duration(response_end_at - self.request_init_at)),
            since_response_init: None,
            response_bytes: 0,
            eos: Some(api::Eos {
                end: reason.map(|r| api::eos::End::ResetErrorCode(r.into())),
            }),
        });

        let event = api::TapEvent {
            event: Some(api::tap_event::Event::Http(api::tap_event::Http {
                event: Some(end),
            })),
            ..self.base_event
        };
        let _ = self.tap.tx.try_send(event);
    }
}

// === impl TapRequestPayload ===

impl iface::TapPayload for TapRequestPayload {
    fn data<B: Buf>(&mut self, _: &B) {}

    fn eos(self, _: Option<&http::HeaderMap>) {}

    fn fail<E: HasH2Reason>(self, _: &E) {}
}

// === impl TapResponsePayload ===

impl iface::TapPayload for TapResponsePayload {
    fn data<B: Buf>(&mut self, data: &B) {
        self.response_bytes += data.remaining();
    }

    fn eos(self, trls: Option<&http::HeaderMap>) {
        let status = match trls {
            None => self.grpc_status,
            Some(t) => t
                .get("grpc-status")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u32>().ok()),
        };

        self.send(status.map(api::eos::End::GrpcStatusCode));
    }

    fn fail<E: HasH2Reason>(self, e: &E) {
        let end = e.h2_reason().map(|r| api::eos::End::ResetErrorCode(r.into()));
        self.send(end);
    }
}

impl TapResponsePayload {
    fn send(mut self, end: Option<api::eos::End>) {
        let response_end_at = clock::now();
        let end = api::tap_event::http::ResponseEnd {
            id: Some(self.tap.id),
            since_request_init: Some(pb_duration(response_end_at - self.request_init_at)),
            since_response_init: Some(pb_duration(response_end_at - self.response_init_at)),
            response_bytes: self.response_bytes as u64,
            eos: Some(api::Eos { end }),
        };

        let event = api::TapEvent {
            event: Some(api::tap_event::Event::Http(api::tap_event::Http {
                event: Some(api::tap_event::http::Event::ResponseEnd(end)),
            })),
            ..self.base_event
        };
        let _ = self.tap.tx.try_send(event);
    }
}

// All of the events emitted from tap have a common set of metadata.
// Build this once, without an `event`, so that it can be used to build
// each HTTP event.
fn base_event<B, I: Inspect>(req: &http::Request<B>, inspect: &I) -> api::TapEvent {
    api::TapEvent {
        proxy_direction: if inspect.is_outbound(req) {
            api::tap_event::ProxyDirection::Outbound.into()
        } else {
            api::tap_event::ProxyDirection::Inbound.into()
        },
        source: inspect.src_addr(req).as_ref().map(|a| a.into()),
        source_meta: {
            let mut m = api::tap_event::EndpointMeta::default();
            let tls = format!("{}", inspect.src_tls(req));
            m.labels.insert("tls".to_owned(), tls);
            Some(m)
        },
        destination: inspect.dst_addr(req).as_ref().map(|a| a.into()),
        destination_meta: inspect.dst_labels(req).map(|labels| {
            let mut m = api::tap_event::EndpointMeta::default();
            m.labels.extend(labels.iter().map(|(k, v)| (k.clone(), v.clone())));
            let tls = format!("{}", inspect.dst_tls(req));
            m.labels.insert("tls".to_owned(), tls);
            m
        }),
        route_meta: inspect.route_labels(req).map(|labels| {
            let mut m = api::tap_event::RouteMeta::default();
            m.labels.extend(labels.as_ref().iter().map(|(k, v)| (k.clone(), v.clone())));
            m
        }),
        event: None,
    }
}
