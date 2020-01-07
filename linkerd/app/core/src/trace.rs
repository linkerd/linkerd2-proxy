use linkerd2_error::Error;
use std::{env, fmt, str, time::Instant};
use tokio_timer::clock;
use tracing::{Dispatch, Event, Level};
use tracing_subscriber::{
    filter,
    fmt::{format, Builder, Context, Formatter},
    reload, EnvFilter, FmtSubscriber,
};

const ENV_LOG: &str = "LINKERD2_PROXY_LOG";

type SubscriberBuilder = Builder<format::NewRecorder, Format, filter::LevelFilter>;
type Subscriber = Formatter<format::NewRecorder, Format>;

#[derive(Clone)]
pub struct LevelHandle {
    inner: reload::Handle<EnvFilter, Subscriber>,
}

/// Initialize tracing and logging with the value of the `ENV_LOG`
/// environment variable as the verbosity-level filter.
pub fn init() -> Result<LevelHandle, Error> {
    let env = env::var(ENV_LOG).unwrap_or_default();
    let (dispatch, handle) = with_filter(env);

    // Set up log compatibility.
    init_log_compat()?;
    // Set the default subscriber.
    tracing::dispatcher::set_global_default(dispatch)?;
    Ok(handle)
}

pub fn init_log_compat() -> Result<(), Error> {
    tracing_log::LogTracer::init().map_err(Error::from)
}

pub fn with_filter(filter: impl AsRef<str>) -> (Dispatch, LevelHandle) {
    let filter = filter.as_ref();

    // Set up the subscriber
    let builder = subscriber_builder()
        .with_env_filter(filter)
        .with_filter_reloading();
    let handle = LevelHandle {
        inner: builder.reload_handle(),
    };
    let dispatch = Dispatch::new(builder.finish());

    (dispatch, handle)
}

/// Returns a builder that constructs a `FmtSubscriber` that logs trace events.
fn subscriber_builder() -> SubscriberBuilder {
    let start_time = clock::now();
    FmtSubscriber::builder().on_event(Format { start_time })
}

struct Format {
    start_time: Instant,
}

impl<N> tracing_subscriber::fmt::FormatEvent<N> for Format
where
    N: for<'a> tracing_subscriber::fmt::NewVisitor<'a>,
{
    fn format_event(
        &self,
        span_ctx: &Context<'_, N>,
        f: &mut dyn fmt::Write,
        event: &Event<'_>,
    ) -> fmt::Result {
        use tracing_log::NormalizeEvent;
        // If the event was converted from a `log` record, use the
        // normalized tracing metadata for that log record.
        let norm_meta = event.normalized_metadata();
        let meta = norm_meta.as_ref().unwrap_or_else(|| event.metadata());

        let level = match meta.level() {
            &Level::TRACE => "TRCE",
            &Level::DEBUG => "DBUG",
            &Level::INFO => "INFO",
            &Level::WARN => "WARN",
            &Level::ERROR => "ERR!",
        };
        let uptime = clock::now() - self.start_time;
        write!(
            f,
            "{} [{:>6}.{:06}s] {}{} ",
            level,
            uptime.as_secs(),
            uptime.subsec_micros(),
            SpanContext(&span_ctx),
            meta.target()
        )?;
        {
            let mut recorder = span_ctx.new_visitor(f, true);
            event.record(&mut recorder);
        }
        writeln!(f)
    }
}

impl LevelHandle {
    /// Returns a new `LevelHandle` without a corresponding filter.
    ///
    /// This will do nothing, but is required for admin endpoint tests which
    /// do not exercise the `proxy-log-level` endpoint.
    pub fn dangling() -> Self {
        let builder = subscriber_builder()
            .with_env_filter(EnvFilter::default())
            .with_filter_reloading();
        let inner = builder.reload_handle();
        LevelHandle { inner }
    }

    pub fn set_level(&self, level: impl AsRef<str>) -> Result<(), Error> {
        let level = level.as_ref();
        let filter = level.parse::<EnvFilter>()?;
        self.inner.reload(filter)?;
        tracing::info!(%level, "set new log level");
        Ok(())
    }

    pub fn current(&self) -> Result<String, Error> {
        self.inner
            .with_current(|f| format!("{}", f))
            .map_err(Into::into)
    }
}

impl fmt::Debug for LevelHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner
            .with_current(|c| {
                f.debug_struct("LevelHandle")
                    .field("current", &format_args!("{}", c))
                    .finish()
            })
            .unwrap_or_else(|e| {
                f.debug_struct("LevelHandle")
                    .field("current", &format_args!("{}", e))
                    .finish()
            })
    }
}

/// Implements `fmt::Display` for a `tokio-trace-fmt` span context.
struct SpanContext<'a, N>(&'a Context<'a, N>);

impl<'a, N> fmt::Display for SpanContext<'a, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut seen = false;
        self.0.visit_spans(|_, span| {
            write!(f, "{}", span.name())?;
            seen = true;

            let fields = span.fields();
            if !fields.is_empty() {
                write!(f, "{{{}}}", fields)?;
            }
            ":".fmt(f)
        })?;
        if seen {
            f.pad(" ")?;
        }
        Ok(())
    }
}

pub trait GetSpan<T> {
    fn get_span(&self, target: &T) -> tracing::Span;
}

impl<T, F> GetSpan<T> for F
where
    F: Fn(&T) -> tracing::Span,
{
    fn get_span(&self, target: &T) -> tracing::Span {
        (self)(target)
    }
}

impl<T: GetSpan<()>> GetSpan<T> for () {
    fn get_span(&self, t: &T) -> tracing::Span {
        t.get_span(&())
    }
}

impl<T> GetSpan<T> for tracing::Span {
    fn get_span(&self, _: &T) -> tracing::Span {
        self.clone()
    }
}

pub use self::layer::Layer;

pub mod layer {
    use super::GetSpan;
    use futures::{future, Async, Future, Poll};
    use linkerd2_error::Error;
    use linkerd2_stack::{NewService, Proxy};
    use tracing::{info_span, trace, Span};
    use tracing_futures::{Instrument, Instrumented};

    #[derive(Clone, Debug)]
    pub struct Layer<G> {
        get_span: G,
    }

    #[derive(Clone, Debug)]
    pub struct MakeSpan<G, M> {
        get_span: G,
        make: M,
    }

    #[derive(Clone, Debug)]
    pub struct SetSpan<S> {
        span: Span,
        inner: S,
    }

    impl<G> Layer<G> {
        pub fn new(get_span: G) -> Self {
            Self { get_span }
        }
    }

    impl Layer<()> {
        pub fn from_target() -> Self {
            Self::new(())
        }
    }

    impl Default for Layer<Span> {
        fn default() -> Self {
            Self::new(Span::current())
        }
    }

    impl<G: Clone, M> tower::layer::Layer<M> for Layer<G> {
        type Service = MakeSpan<G, M>;

        fn layer(&self, make: M) -> Self::Service {
            Self::Service {
                make,
                get_span: self.get_span.clone(),
            }
        }
    }

    impl<T, G, N> NewService<T> for MakeSpan<G, N>
    where
        T: std::fmt::Debug,
        G: GetSpan<T>,
        N: NewService<T>,
    {
        type Service = SetSpan<N::Service>;

        fn new_service(&self, target: T) -> Self::Service {
            trace!(?target, "new_service");

            let span = self.get_span.get_span(&target);
            let inner = {
                let _enter = span.enter();
                self.make.new_service(target)
            };

            SetSpan { inner, span }
        }
    }

    impl<T, G, M> tower::Service<T> for MakeSpan<G, M>
    where
        T: std::fmt::Debug,
        G: GetSpan<T>,
        M: tower::Service<T>,
        M::Error: Into<Error>,
    {
        type Response = SetSpan<M::Response>;
        type Error = Error;
        type Future = SetSpan<M::Future>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            let span = info_span!("make_service");
            let _enter = span.enter();

            trace!("poll_ready");
            match self.make.poll_ready() {
                Err(e) => {
                    let error = e.into();
                    trace!(%error);
                    Err(error)
                }
                Ok(ready) => {
                    trace!(ready = ready.is_ready());
                    Ok(ready)
                }
            }
        }

        fn call(&mut self, target: T) -> Self::Future {
            info_span!("make_service").in_scope(|| trace!(?target, "call"));

            let span = self.get_span.get_span(&target);
            let inner = {
                let _enter = span.enter();
                self.make.call(target)
            };

            SetSpan { inner, span }
        }
    }

    impl<F> Future for SetSpan<F>
    where
        F: Future,
        F::Error: Into<Error>,
    {
        type Item = SetSpan<F::Item>;
        type Error = Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let _enter = self.span.enter();
            let span = info_span!("making");
            let _enter = span.enter();

            match self.inner.poll() {
                Err(e) => {
                    let error = e.into();
                    trace!(%error);
                    Err(error)
                }
                Ok(Async::NotReady) => {
                    trace!(ready = false);
                    Ok(Async::NotReady)
                }
                Ok(Async::Ready(inner)) => {
                    trace!(ready = true);
                    let svc = SetSpan {
                        inner,
                        span: self.span.clone(),
                    };
                    Ok(svc.into())
                }
            }
        }
    }

    impl<Req, S, P> Proxy<Req, S> for SetSpan<P>
    where
        Req: std::fmt::Debug,
        P: Proxy<Req, S>,
        S: tower::Service<P::Request>,
    {
        type Request = P::Request;
        type Response = P::Response;
        type Error = P::Error;
        type Future = Instrumented<P::Future>;

        fn proxy(&self, svc: &mut S, request: Req) -> Self::Future {
            let _enter = self.span.enter();
            trace!(?request, "proxy");
            self.inner.proxy(svc, request).instrument(self.span.clone())
        }
    }

    impl<Req, S> tower::Service<Req> for SetSpan<S>
    where
        Req: std::fmt::Debug,
        S: tower::Service<Req>,
        S::Error: Into<Error>,
    {
        type Response = S::Response;
        type Error = Error;
        type Future = future::MapErr<Instrumented<S::Future>, fn(S::Error) -> Error>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            let _enter = self.span.enter();

            trace!("poll ready");
            match self.inner.poll_ready() {
                Err(e) => {
                    let error = e.into();
                    trace!(%error);
                    Err(error)
                }
                Ok(ready) => {
                    trace!(ready = ready.is_ready());
                    Ok(ready)
                }
            }
        }

        fn call(&mut self, request: Req) -> Self::Future {
            let _enter = self.span.enter();

            trace!(?request, "call");
            self.inner
                .call(request)
                .instrument(self.span.clone())
                .map_err(Into::into)
        }
    }
}
