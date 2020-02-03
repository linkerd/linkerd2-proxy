const ENV_LOG: &str = "LINKERD2_PROXY_LOG";

use linkerd2_error::Error;
use std::{env, fmt, str, time::Instant};
use tokio_timer::clock;
use tracing::Dispatch;
use tracing_subscriber::{
    fmt::{format, Formatter},
    reload, EnvFilter, FmtSubscriber,
};

type Subscriber = Formatter<format::DefaultFields, format::Format<format::Full, Uptime>>;

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
    let start_time = clock::now();
    let builder = FmtSubscriber::builder()
        .with_timer(Uptime { start_time })
        .with_env_filter(filter)
        .with_filter_reloading()
        .with_ansi(cfg!(test));
    let handle = LevelHandle {
        inner: builder.reload_handle(),
    };
    let dispatch = Dispatch::new(builder.finish());

    (dispatch, handle)
}

struct Uptime {
    start_time: Instant,
}

impl tracing_subscriber::fmt::time::FormatTime for Uptime {
    fn format_time(&self, w: &mut dyn fmt::Write) -> fmt::Result {
        let uptime = clock::now() - self.start_time;
        write!(w, "[{:>6}.{:06}s]", uptime.as_secs(), uptime.subsec_nanos())
    }
}

impl LevelHandle {
    /// Returns a new `LevelHandle` without a corresponding filter.
    ///
    /// This will do nothing, but is required for admin endpoint tests which
    /// do not exercise the `proxy-log-level` endpoint.
    pub fn dangling() -> Self {
        let (_, handle) = with_filter("");
        handle
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

impl<T> GetSpan<T> for tracing::Span {
    fn get_span(&self, _: &T) -> tracing::Span {
        self.clone()
    }
}

pub mod layer {
    use super::GetSpan;
    use futures::{future, Future, Poll};
    use tracing::Span;
    use tracing_futures::{Instrument, Instrumented};

    pub struct Layer<T, G: GetSpan<T>> {
        get_span: G,
        _marker: std::marker::PhantomData<fn(T)>,
    }

    pub struct Make<T, G: GetSpan<T>, M: tower::Service<T>> {
        get_span: G,
        make: M,
        _marker: std::marker::PhantomData<fn(T)>,
    }

    pub struct Service<S> {
        span: Span,
        service: S,
    }

    impl<T, G: GetSpan<T> + Clone> Layer<T, G> {
        pub fn new(get_span: G) -> Self {
            Self {
                get_span,
                _marker: std::marker::PhantomData,
            }
        }
    }

    impl<T> Default for Layer<T, Span> {
        fn default() -> Self {
            Self::new(Span::current())
        }
    }

    impl<T, G: GetSpan<T> + Clone> Clone for Layer<T, G> {
        fn clone(&self) -> Self {
            Self {
                get_span: self.get_span.clone(),
                _marker: std::marker::PhantomData,
            }
        }
    }

    impl<T, G: GetSpan<T> + Clone, M: tower::Service<T>> tower::layer::Layer<M> for Layer<T, G> {
        type Service = Make<T, G, M>;

        fn layer(&self, make: M) -> Self::Service {
            Self::Service {
                make,
                get_span: self.get_span.clone(),
                _marker: std::marker::PhantomData,
            }
        }
    }

    impl<T, G: GetSpan<T> + Clone, M: tower::Service<T> + Clone> Clone for Make<T, G, M> {
        fn clone(&self) -> Self {
            Self {
                make: self.make.clone(),
                get_span: self.get_span.clone(),
                _marker: std::marker::PhantomData,
            }
        }
    }

    impl<T, G: GetSpan<T>, M: tower::Service<T>> tower::Service<T> for Make<T, G, M> {
        type Response = Service<M::Response>;
        type Error = M::Error;
        type Future = Instrumented<future::Map<M::Future, fn(M::Response) -> Service<M::Response>>>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.make.poll_ready()
        }

        fn call(&mut self, target: T) -> Self::Future {
            let span = self.get_span.get_span(&target);
            let _enter = span.enter();

            // `span` is not passed through to avoid making `new_svc` capture...
            let new_svc: fn(M::Response) -> Service<M::Response> = |service| Service {
                service,
                span: Span::current(),
            };
            self.make.call(target).map(new_svc).instrument(span.clone())
        }
    }

    impl<S: Clone> Clone for Service<S> {
        fn clone(&self) -> Self {
            Self {
                service: self.service.clone(),
                span: self.span.clone(),
            }
        }
    }

    impl<Req, S: tower::Service<Req>> tower::Service<Req> for Service<S> {
        type Response = S::Response;
        type Error = S::Error;
        type Future = Instrumented<S::Future>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            let _enter = self.span.enter();
            self.service.poll_ready()
        }

        fn call(&mut self, req: Req) -> Self::Future {
            let _enter = self.span.enter();
            self.service.call(req).instrument(self.span.clone())
        }
    }
}

pub use self::layer::Layer;

pub fn layer<T, G: GetSpan<T> + Clone>(get_span: G) -> Layer<T, G> {
    Layer::new(get_span)
}
