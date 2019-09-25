use super::match_::Match;
use crate::api::{http_types, pb_duration, tap as api};
use crate::proxy::http::HasH2Reason;
use crate::tap::{iface, Inspect};
use crate::transport::tls;
use crate::Conditional;
use bytes::Buf;
use futures::sync::mpsc;
use futures::{future, Async, Future, Poll, Stream};
use hyper::body::Payload;
use std::convert::TryFrom;
use std::iter;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;
use tokio_timer::clock;
use tower_grpc::{self as grpc, Response};
use tracing::{debug, trace, warn};

#[derive(Clone, Debug)]
pub struct Server<T> {
    subscribe: T,
    base_id: Arc<AtomicUsize>,
}

#[derive(Debug)]
pub struct ResponseFuture<F> {
    subscribe: F,
    events_rx: Option<mpsc::Receiver<api::TapEvent>>,
    shared: Option<Arc<Shared>>,
}

#[derive(Debug)]
pub struct ResponseStream {
    events_rx: mpsc::Receiver<api::TapEvent>,
    shared: Option<Arc<Shared>>,
}

#[derive(Debug)]
struct Shared {
    base_id: u32,
    count: AtomicUsize,
    limit: usize,
    match_: Match,
    extract: Extract,
    events_tx: mpsc::Sender<api::TapEvent>,
}

#[derive(Clone, Debug)]
struct TapTx {
    id: api::tap_event::http::StreamId,
    tx: mpsc::Sender<api::TapEvent>,
}

#[derive(Clone, Debug)]
pub struct Tap {
    shared: Weak<Shared>,
}

#[derive(Debug)]
pub struct TapResponse {
    base_event: api::TapEvent,
    request_init_at: Instant,
    /// Should headers be extracted?
    extract_headers: bool,
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
    /// Should headers be extracted?
    extract_headers: bool,
    // Response-headers may include grpc-status when there is no response body.
    grpc_status: Option<u32>,
}

#[derive(Debug)]
enum Extract {
    Http { headers: bool },
}

// === impl Server ===

impl<T: iface::Subscribe<Tap>> Server<T> {
    pub(in crate::tap) fn new(subscribe: T) -> Self {
        let base_id = Arc::new(0.into());
        Self { base_id, subscribe }
    }

    fn invalid_arg(message: String) -> grpc::Status {
        grpc::Status::new(grpc::Code::InvalidArgument, message)
    }
}

impl<T> api::server::Tap for Server<T>
where
    T: iface::Subscribe<Tap> + Clone,
{
    type ObserveStream = ResponseStream;
    type ObserveFuture = future::Either<
        future::FutureResult<Response<Self::ObserveStream>, grpc::Status>,
        ResponseFuture<T::Future>,
    >;

    fn observe(&mut self, req: grpc::Request<api::ObserveRequest>) -> Self::ObserveFuture {
        let req = req.into_inner();

        let limit = req.limit as usize;
        if limit == 0 {
            let err = Self::invalid_arg("limit must be positive".into());
            return future::Either::A(future::err(err));
        };
        trace!("tap: limit={}", limit);

        // Read the match logic into a type we can use to evaluate against
        // requests. This match will be shared (weakly) by all registered
        // services to match requests. The response stream strongly holds the
        // match until the response is complete. This way, services never
        // evaluate matches for taps that have been completed or canceled.
        let match_ = match Match::try_new(req.r#match) {
            Ok(m) => m,
            Err(e) => {
                warn!("invalid tap request: {} ", e);
                let err = Self::invalid_arg(e.to_string());
                return future::Either::A(future::err(err));
            }
        };

        let extract = match req.extract.and_then(|ex| Extract::try_from(ex).ok()) {
            Some(ex) => ex,
            None => {
                // If there's no extract field, the request may have been sent
                // by an older version of the Linkerd control plane. If this is
                // the case, rather than failing the tap, just do the only
                // behavior that older control planes know about --- extract
                // HTTP data without headers.
                let default = Extract::Http { headers: false };
                warn!(?default, "tap request with no extract field; using default");
                default
            }
        };

        // Wrapping is okay. This is realy just to disambiguate events within a
        // single tap session (i.e. that may consist of several tap requests).
        let base_id = self.base_id.fetch_add(1, Ordering::Relaxed) as u32;
        debug!(id = ?base_id, r#match = ?match_, ?extract, "tap;");

        // The events channel is used to emit tap events to the response stream.
        //
        // At most `limit` copies of `events_tx` are dispatched to `taps_rx`
        // requests. Each tapped request's sender is dropped when the response
        // completes, so the event stream closes gracefully when all tapped
        // requests are completed without additional coordination.
        let (events_tx, events_rx) =
            mpsc::channel(super::super::PER_RESPONSE_EVENT_BUFFER_CAPACITY);

        let shared = Arc::new(Shared {
            base_id,
            count: AtomicUsize::new(0),
            limit,
            match_,
            extract,
            events_tx,
        });

        let tap = Tap {
            shared: Arc::downgrade(&shared),
        };
        let subscribe = self.subscribe.subscribe(tap);

        // Reads up to `limit` requests from from `taps_rx` and satisfies them
        // with a cpoy of `events_tx`.

        future::Either::B(ResponseFuture {
            subscribe,
            shared: Some(shared),
            events_rx: Some(events_rx),
        })
    }
}

impl<F: Future<Item = ()>> Future for ResponseFuture<F> {
    type Item = Response<ResponseStream>;
    type Error = grpc::Status;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Ensure that tap registers successfully.
        match self.subscribe.poll() {
            Ok(Async::NotReady) => return Ok(Async::NotReady),
            Ok(Async::Ready(())) => {}
            Err(_) => {
                let status =
                    grpc::Status::new(grpc::Code::ResourceExhausted, "Too many active taps");
                return Err(status);
            }
        }

        let rsp = ResponseStream {
            shared: self.shared.take(),
            events_rx: self.events_rx.take().expect("events_rx must be set"),
        };

        Ok(Response::new(rsp).into())
    }
}

// === impl ResponseStream ===

impl Stream for ResponseStream {
    type Item = api::TapEvent;
    type Error = grpc::Status;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        // Drop the Shared handle once at our limit so that services do not do
        // any more matching against this tap.
        //
        // Furthermore, this drops the event sender so that `events_rx` closes
        // gracefully when all open taps are complete.
        self.shared = self.shared.take().and_then(|shared| {
            if shared.is_under_limit() {
                Some(shared)
            } else {
                None
            }
        });

        // Read events from taps. The receiver can't actually error, but we need
        // to satisfy the type signature, so we coerce errors into EOS.
        self.events_rx.poll().or_else(|_| Ok(None.into()))
    }
}

// === impl Shared ===

impl Shared {
    fn is_under_limit(&self) -> bool {
        self.count.load(Ordering::Relaxed) < self.limit
    }
}

// === impl Tap ===

impl iface::Tap for Tap {
    type TapRequestPayload = TapRequestPayload;
    type TapResponse = TapResponse;
    type TapResponsePayload = TapResponsePayload;

    fn can_tap_more(&self) -> bool {
        self.shared
            .upgrade()
            .map(|shared| shared.is_under_limit())
            .unwrap_or(false)
    }

    fn tap<B, I>(
        &mut self,
        req: &http::Request<B>,
        inspect: &I,
    ) -> Option<(TapRequestPayload, TapResponse)>
    where
        B: Payload,
        I: Inspect,
    {
        let (id, mut events_tx, extract_headers) = self.shared.upgrade().and_then(|shared| {
            if !shared.match_.matches(req, inspect) {
                return None;
            }
            let next_id = shared.count.fetch_add(1, Ordering::Relaxed);
            if next_id < shared.limit {
                let id = api::tap_event::http::StreamId {
                    base: shared.base_id,
                    stream: next_id as u64,
                };
                // Is HTTP data being extracted from the request, and should
                // headers be included?
                match shared.extract {
                    Extract::Http { headers } => {
                        return Some((id, shared.events_tx.clone(), headers))
                    }
                    // _ => {}
                }
            }
            None
        })?;

        let request_init_at = clock::now();

        let base_event = base_event(req, inspect);

        let authority = inspect.authority(req).unwrap_or_default();

        let headers = if extract_headers {
            let headers = if req.version() == http::Version::HTTP_2 {
                // If the request is HTTP/2, add the pseudo-header fields to the
                // headers.
                let pseudos = vec![
                    http_types::headers::Header {
                        name: ":method".to_owned(),
                        value: req.method().as_str().as_bytes().into(),
                    },
                    http_types::headers::Header {
                        name: ":scheme".to_owned(),
                        value: req
                            .uri()
                            .scheme_str()
                            .map(|s| s.as_bytes().into())
                            .unwrap_or_default(),
                    },
                    http_types::headers::Header {
                        name: ":authority".to_owned(),
                        value: authority.as_bytes().into(),
                    },
                    http_types::headers::Header {
                        name: ":path".to_owned(),
                        value: req
                            .uri()
                            .path_and_query()
                            .map(|p| p.as_str().as_bytes().into())
                            .unwrap_or_default(),
                    },
                ];
                headers_to_pb(pseudos, req.headers())
            } else {
                headers_to_pb(iter::empty(), req.headers())
            };
            Some(headers)
        } else {
            None
        };

        let init = api::tap_event::http::RequestInit {
            id: Some(id.clone()),
            method: Some(req.method().into()),
            scheme: req.uri().scheme_part().map(http_types::Scheme::from),
            authority,
            path: req.uri().path().into(),
            headers,
        };

        let event = api::TapEvent {
            event: Some(api::tap_event::Event::Http(api::tap_event::Http {
                event: Some(api::tap_event::http::Event::RequestInit(init)),
            })),
            ..base_event.clone()
        };

        // If try_send fails, just return `None`...
        events_tx.try_send(event).ok()?;

        let tap = TapTx { id, tx: events_tx };

        let req = TapRequestPayload {
            tap: tap.clone(),
            base_event: base_event.clone(),
        };
        let rsp = TapResponse {
            tap,
            base_event,
            request_init_at,
            extract_headers,
        };
        Some((req, rsp))
    }
}

// === impl TapResponse ===

impl iface::TapResponse for TapResponse {
    type TapPayload = TapResponsePayload;

    fn tap<B: Payload>(mut self, rsp: &http::Response<B>) -> TapResponsePayload {
        let response_init_at = clock::now();

        let headers = if self.extract_headers {
            let headers = if rsp.version() == http::Version::HTTP_2 {
                let pseudos = iter::once(http_types::headers::Header {
                    name: ":status".to_owned(),
                    value: rsp.status().as_str().as_bytes().into(),
                });
                headers_to_pb(pseudos, rsp.headers())
            } else {
                headers_to_pb(iter::empty(), rsp.headers())
            };
            Some(headers)
        } else {
            None
        };

        let init = api::tap_event::http::Event::ResponseInit(api::tap_event::http::ResponseInit {
            id: Some(self.tap.id.clone()),
            since_request_init: Some(pb_duration(response_init_at - self.request_init_at)),
            http_status: rsp.status().as_u16().into(),
            headers,
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
            extract_headers: self.extract_headers,
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
            trailers: None,
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

        self.send(status.map(api::eos::End::GrpcStatusCode), trls);
    }

    fn fail<E: HasH2Reason>(self, e: &E) {
        let end = e
            .h2_reason()
            .map(|r| api::eos::End::ResetErrorCode(r.into()));
        self.send(end, None);
    }
}

impl TapResponsePayload {
    fn send(mut self, end: Option<api::eos::End>, trls: Option<&http::HeaderMap>) {
        let response_end_at = clock::now();
        let trailers = if self.extract_headers {
            trls.map(|trls| headers_to_pb(iter::empty(), trls))
        } else {
            None
        };
        let end = api::tap_event::http::ResponseEnd {
            id: Some(self.tap.id),
            since_request_init: Some(pb_duration(response_end_at - self.request_init_at)),
            since_response_init: Some(pb_duration(response_end_at - self.response_init_at)),
            response_bytes: self.response_bytes as u64,
            eos: Some(api::Eos { end }),
            trailers,
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

impl TryFrom<api::observe_request::Extract> for Extract {
    type Error = ();
    fn try_from(req: api::observe_request::Extract) -> Result<Self, Self::Error> {
        match req.extract {
            Some(api::observe_request::extract::Extract::Http(inner)) => {
                let headers = match inner.extract {
                    Some(api::observe_request::extract::http::Extract::Headers(_)) => true,
                    _ => false,
                };
                Ok(Extract::Http { headers })
            }
            _ => Err(()),
        }
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
            let tls = inspect.src_tls(req);
            let tls_status = tls::Status::from(tls.as_ref());
            m.labels.insert("tls".to_owned(), tls_status.to_string());
            if let Conditional::Some(id) = tls {
                m.labels
                    .insert("client_id".to_owned(), id.as_ref().to_owned());
            }
            Some(m)
        },
        destination: inspect.dst_addr(req).as_ref().map(|a| a.into()),
        destination_meta: inspect.dst_labels(req).map(|labels| {
            let mut m = api::tap_event::EndpointMeta::default();
            m.labels
                .extend(labels.iter().map(|(k, v)| (k.clone(), v.clone())));
            let tls = inspect.dst_tls(req);
            let tls_status = tls::Status::from(tls.as_ref());
            m.labels.insert("tls".to_owned(), tls_status.to_string());
            if let Conditional::Some(id) = tls {
                m.labels
                    .insert("server_id".to_owned(), id.as_ref().to_owned());
            }
            m
        }),
        route_meta: inspect.route_labels(req).map(|labels| {
            let mut m = api::tap_event::RouteMeta::default();
            m.labels
                .extend(labels.as_ref().iter().map(|(k, v)| (k.clone(), v.clone())));
            m
        }),
        event: None,
    }
}

fn headers_to_pb(
    pseudos: impl IntoIterator<Item = http_types::headers::Header>,
    headers: &http::HeaderMap,
) -> http_types::Headers {
    http_types::Headers {
        headers: pseudos
            .into_iter()
            .chain(
                headers
                    .iter()
                    .map(|(name, value)| http_types::headers::Header {
                        name: name.as_str().to_owned(),
                        value: value.as_bytes().into(),
                    }),
            )
            .collect(),
    }
}
