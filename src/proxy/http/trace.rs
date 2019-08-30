use crate::{Error, svc};
use futures::{try_ready, Async, Future, Poll};
use http::header::HeaderValue;
use rand::Rng;
use std::fmt;
use tokio::sync::mpsc;
use tracing::{trace, warn};
use std::time::SystemTime;

const GRPC_TRACE_HEADER: &str = "grpc-trace-bin";
const GRPC_TRACE_FIELD_TRACE_ID: u8 = 0;
const GRPC_TRACE_FIELD_SPAN_ID: u8 = 1;
const GRPC_TRACE_FIELD_TRACE_OPTIONS: u8 = 2;

#[derive(Debug)]
struct TraceContext {
    version: Id,
    trace_id: Id,
    parent_id: Id,
    flags: Id,
    span_id: Option<Id>,
}

#[derive(Debug, Default)]
pub struct Id(Vec<u8>);

#[derive(Debug)]
struct InsufficientBytes;
#[derive(Debug)]
struct UnknownFieldId(u8);

#[derive(Debug)]
pub struct Span {
    pub trace_id: Id,
    pub span_id: Id,
    pub parent_id: Id,
    pub span_name: String,
    pub start: SystemTime,
    pub end: SystemTime,
}

pub struct SpanFuture<F> {
    span: Option<Span>,
    inner: F,
    sender: mpsc::Sender<Span>,
}

#[derive(Clone, Debug)]
pub struct Layer {
    // TODO: Replace mpsc::Sender with a trait so that we can accept other
    // implementations.
    sender: mpsc::Sender<Span>,
}

#[derive(Clone, Debug)]
pub struct Stack<M> {
    inner: M,
    sender: mpsc::Sender<Span>,
}

pub struct MakeFuture<F> {
    inner: F,
    sender: Option<mpsc::Sender<Span>>,
}

#[derive(Clone, Debug)]
pub struct Service<S> {
    inner: S,
    sender: mpsc::Sender<Span>,
}

/// A layer that adds distributed tracing instrumentation.
///
/// This layer reads the `traceparent` HTTP header from the request.  If this
/// header is absent, the request is fowarded unmodified.  If the header is
/// present, a new span will be started in the current trace by creating a new
/// random span id setting it into the `traceparent` header before forwarding
/// the request.  If the sampled bit of the header was set, we emit metadata
/// about the span to the returned channel when the span is complete, i.e. when
/// we receive the response.
pub fn layer(sender: mpsc::Sender<Span>) -> Layer {
    Layer { sender }
}

// === impl Layer ===

impl<M> svc::Layer<M> for Layer {
    type Service = Stack<M>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack {
            inner,
            sender: self.sender.clone(),
        }
    }
}

// === impl Stack ===

impl<T, M> svc::Service<T> for Stack<M>
where
    M: svc::Service<T>,
{
    type Response = Service<M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), M::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.call(target);

        MakeFuture {
            inner,
            sender: Some(self.sender.clone()),
        }
    }
}

// === impl MakeFuture ===

impl<F: Future> Future for MakeFuture<F> {
    type Item = Service<F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let sender = self.sender.take().expect("poll called after ready");
        Ok(Async::Ready(Service { inner, sender }))
    }
}

// === impl Service ===

impl<S, B> svc::Service<http::Request<B>> for Service<S>
where
    S: svc::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = futures::future::Either<S::Future, SpanFuture<S::Future>>;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        let mut trace_context = unpack_grpc_trace_context(&request);
        let mut path: Option<String> = None;

        if let Some(ref mut context) = trace_context {
            trace!("got trace contex: {:?}", context);
            increment_grpc_span_id(&mut request, context);
            // If we plan to sample this span, we need to copy the path from
            // the request before dispatching it to inner.
            if is_sampled(&context.flags) {
                path = Some(request.uri().path().to_string());
            }
        }

        let f = self.inner.call(request);

        if let Some(TraceContext {
            version: _,
            trace_id,
            parent_id,
            flags,
            span_id: Some(span_id),
        }) = trace_context
        {
            if is_sampled(&flags) {
                trace!("span {:?} will be sampled", span_id);
                let span = Span {
                    trace_id: trace_id,
                    span_id: span_id,
                    parent_id: parent_id,
                    span_name: path.unwrap_or(String::new()),
                    start: SystemTime::now(),
                    // End time will be updated when the span completes.
                    end: SystemTime::now(),
                };
                let span_fut = SpanFuture {
                    span: Some(span),
                    inner: f,
                    sender: self.sender.clone(),
                };
                futures::future::Either::B(span_fut)
            } else {
                futures::future::Either::A(f)
            }
        } else {
            futures::future::Either::A(f)
        }
    }
}

// === impl SpanFuture ===

impl<F: Future> Future for SpanFuture<F> {
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let mut span = self.span.take().expect("span missing");
        span.end = SystemTime::now();
        trace!("emitting span: {:?}", span);
        self.sender.try_send(span).unwrap_or_else(|_| {
            warn!("span dropped due to backpressure");
        });
        Ok(Async::Ready(inner))
    }
}

// === impl Id ===

impl Id {
    fn new(len: usize) -> Self {
        let mut rng = rand::thread_rng();
        let mut bytes = vec![0; len];
        rng.fill(bytes.as_mut_slice());
        Self(bytes)
    }

    fn read_from_slice(len: usize, buf: &mut &[u8]) -> Result<Self, InsufficientBytes> {
        if buf.len() >= len {
            let bytes = Vec::from(&buf[..len]);
            *buf = &buf[len..];
            Ok(Self(bytes))
        } else {
            Err(InsufficientBytes)
        }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0.iter() {
            write!(f, "{:02x?}", b)?;
        }
        Ok(())
    }
}

// === impl InsufficientBytes ===

impl std::error::Error for InsufficientBytes {}

impl fmt::Display for InsufficientBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Insufficient bytes when decoding binary header")
    }
}

// === impl UnknownFieldId ===

impl std::error::Error for UnknownFieldId {}

impl fmt::Display for UnknownFieldId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Unknown field id {}", self.0)
    }
}

fn unpack_grpc_trace_context<B>(request: &http::Request<B>) -> Option<TraceContext> {
    request
        .headers()
        .get(GRPC_TRACE_HEADER)
        .and_then(|trace_context| {
            let mut bytes = trace_context.as_bytes();
            parse_grpc_trace_context_fields(&mut bytes)
        })
}

fn parse_grpc_trace_context_fields(buf: &mut &[u8]) -> Option<TraceContext> {
    let version = Id::read_from_slice(1, buf).ok()?;

    let mut context = TraceContext {
        version: version,
        trace_id: Default::default(),
        parent_id: Default::default(),
        flags: Default::default(),
        span_id: None,
    };

    while buf.len() > 0 {
        match parse_grpc_trace_context_field(buf, &mut context) {
            Ok(()) => {},
            Err(ref e) if e.is::<UnknownFieldId>() => break,
            Err(e) => {
                warn!("error parsing {} header: {}", GRPC_TRACE_HEADER, e);
                return None;
            },
        };
    }
    Some(context)
}

fn parse_grpc_trace_context_field(buf: &mut &[u8], context: &mut TraceContext) -> Result<(), Error> {
    let field_id = buf[0];
    *buf = &buf[1..];
    match field_id {
        GRPC_TRACE_FIELD_SPAN_ID => {
            let id = Id::read_from_slice(8, buf)?;
            context.parent_id = id;
        },
        GRPC_TRACE_FIELD_TRACE_ID => {
            let id = Id::read_from_slice(16, buf)?;
            context.trace_id = id;
        },
        GRPC_TRACE_FIELD_TRACE_OPTIONS => {
            let flags = Id::read_from_slice(1, buf)?;
            context.flags = flags;
        },
        id => {
            return Err(UnknownFieldId(id).into());
        },
    };
    Ok(())
}

fn increment_grpc_span_id<B>(request: &mut http::Request<B>, context: &mut TraceContext) {
    let span_id = Id::new(8);

    let mut bytes = Vec::<u8>::new();

    // version
    bytes.push(0);

    // trace id
    bytes.push(GRPC_TRACE_FIELD_TRACE_ID);
    bytes.extend(context.trace_id.0.iter());

    // span id
    bytes.push(GRPC_TRACE_FIELD_SPAN_ID);
    bytes.extend(span_id.0.iter());

    // trace options
    bytes.push(GRPC_TRACE_FIELD_TRACE_OPTIONS);
    bytes.extend(context.flags.0.iter());

    trace!("incremented span id: {}", span_id);

    if let Result::Ok(hv) = HeaderValue::from_bytes(bytes.as_ref()) {
        request.headers_mut().insert(GRPC_TRACE_HEADER, hv);
    } else {
        warn!("invalid binary header: {:?}", bytes);
    }
    context.span_id = Some(span_id);
}

fn is_sampled(flags: &Id) -> bool {
    if flags.0.len() != 1 {
        warn!("invalid trace flags: {:?}", flags);
        return false
    }
    flags.0.first().copied().unwrap_or(0) & 1 == 1
}
