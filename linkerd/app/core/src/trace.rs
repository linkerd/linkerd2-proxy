use linkerd2_error::Error;
use std::{env, fmt, str, time::Instant};
use tokio_timer::clock;
use tokio_trace::tasks::{TaskList, TasksLayer};
use tracing::Dispatch;
use tracing_subscriber::{
    fmt::{format, Layer as FmtLayer},
    layer::Layered,
    prelude::*,
    reload, EnvFilter,
};

#[cfg(feature = "flamegraph")]
use std::{fs::File, io::BufWriter};

const ENV_LOG_LEVEL: &str = "LINKERD2_PROXY_LOG";
const ENV_LOG_FORMAT: &str = "LINKERD2_PROXY_LOG_FORMAT";

const DEFAULT_LOG_LEVEL: &str = "warn,linkerd2_proxy=info";
const DEFAULT_LOG_FORMAT: &str = "PLAIN";

type JsonFormatter = Formatter<format::JsonFields, format::Json>;
type PlainFormatter = Formatter<format::DefaultFields, format::Full>;
type Formatter<F, E> = Layered<
    FmtLayer<Layered<TasksLayer<F>, tracing_subscriber::Registry>, F, format::Format<E, Uptime>>,
    Layered<TasksLayer<F>, tracing_subscriber::Registry>,
>;

#[cfg(feature = "flamegraph")]
pub type FlushFlamegraph = tracing_flame::FlushGuard<BufWriter<File>>;

#[cfg(feature = "flamegraph")]
const FLAMEGRAPH_PATH: &str = "/tmp/linkerd2.folded";

#[cfg(not(feature = "flamegraph"))]
pub type FlushFlamegraph = ();

#[derive(Clone)]
pub struct Handle {
    pub level: LevelHandle,
    pub tasks: TaskList,
}

#[derive(Clone)]
pub enum LevelHandle {
    Json(reload::Handle<EnvFilter, JsonFormatter>),
    Plain(reload::Handle<EnvFilter, PlainFormatter>),
}

/// Initialize tracing and logging with the value of the `ENV_LOG`
/// environment variable as the verbosity-level filter.
pub fn init() -> Result<(Handle, FlushFlamegraph), Error> {
    let log_level = env::var(ENV_LOG_LEVEL).unwrap_or(DEFAULT_LOG_LEVEL.to_string());
    let log_format = env::var(ENV_LOG_FORMAT).unwrap_or(DEFAULT_LOG_FORMAT.to_string());

    let (dispatch, handle, flush) = with_filter_and_format(log_level, log_format);

    // Set up log compatibility.
    init_log_compat()?;
    // Set the default subscriber.
    tracing::dispatcher::set_global_default(dispatch)?;
    Ok((handle, flush))
}

pub fn init_log_compat() -> Result<(), Error> {
    tracing_log::LogTracer::init().map_err(Error::from)
}

pub fn with_filter_and_format(
    filter: impl AsRef<str>,
    format: impl AsRef<str>,
) -> (Dispatch, Handle, FlushFlamegraph) {
    let filter = filter.as_ref();

    // Set up the subscriber
    let start_time = clock::now();
    let filter = tracing_subscriber::EnvFilter::new(filter);

    #[cfg(not(feature = "flamegraph"))]
    let flush = ();

    let formatter = tracing_subscriber::fmt::format()
        .with_timer(Uptime { start_time })
        .with_thread_ids(true);
    let (dispatch, level, tasks, flush) = match format.as_ref().to_uppercase().as_ref() {
        "JSON" => {
            let (tasks, tasks_layer) = TasksLayer::<format::JsonFields>::new();
            let (filter, level) = tracing_subscriber::reload::Layer::new(filter);
            #[cfg(feature = "flamegraph")]
            let (flamegraph_layer, flush) = {
                let (layer, flush) = tracing_flame::FlameLayer::with_file(FLAMEGRAPH_PATH)
                    .expect("failed to create file for flamegraph!");
                let layer = layer.with_empty_samples(false).with_threads_collapsed(true);
                (layer, flush)
            };

            let builder = tracing_subscriber::registry()
                .with(tasks_layer)
                .with(
                    tracing_subscriber::fmt::layer()
                        .event_format(formatter)
                        .json()
                        .with_span_list(true),
                )
                .with(filter);
            #[cfg(feature = "flamegraph")]
            let builder = builder.with(flamegraph_layer);

            let level = LevelHandle::Json(level);
            (builder.into(), level, tasks, flush)
        }
        "PLAIN" | _ => {
            let (tasks, tasks_layer) = TasksLayer::<format::DefaultFields>::new();
            let (filter, level) = tracing_subscriber::reload::Layer::new(filter);
            #[cfg(feature = "flamegraph")]
            let (flamegraph_layer, flush) = {
                let (layer, flush) = tracing_flame::FlameLayer::with_file(FLAMEGRAPH_PATH)
                    .expect("failed to create file for flamegraph!");
                let layer = layer.with_empty_samples(false).with_threads_collapsed(true);
                (layer, flush)
            };

            let builder = tracing_subscriber::registry()
                .with(tasks_layer)
                .with(tracing_subscriber::fmt::layer().event_format(formatter))
                .with(filter);
            #[cfg(feature = "flamegraph")]
            let builder = builder.with(flamegraph_layer);

            let level = LevelHandle::Plain(level);
            (builder.into(), level, tasks, flush)
        }
    };

    (dispatch, Handle { level, tasks }, flush)
}

pub struct Uptime {
    start_time: Instant,
}

impl tracing_subscriber::fmt::time::FormatTime for Uptime {
    fn format_time(&self, w: &mut dyn fmt::Write) -> fmt::Result {
        let uptime = clock::now() - self.start_time;
        write!(w, "[{:>6}.{:06}s]", uptime.as_secs(), uptime.subsec_nanos())
    }
}

impl Handle {
    /// Returns a new `LevelHandle` without a corresponding filter.
    ///
    /// This will do nothing, but is required for admin endpoint tests which
    /// do not exercise the `proxy-log-level` endpoint.
    pub fn dangling() -> Self {
        let (_, handle, _) = with_filter_and_format("", "");
        handle
    }
}

impl LevelHandle {
    pub fn set_level(&self, level: impl AsRef<str>) -> Result<(), Error> {
        let level = level.as_ref();
        let filter = level.parse::<EnvFilter>()?;
        match self {
            Self::Json(level) => level.reload(filter)?,
            Self::Plain(level) => level.reload(filter)?,
        }
        tracing::info!(%level, "set new log level");
        Ok(())
    }

    pub fn current(&self) -> Result<String, Error> {
        match self {
            Self::Json(handle) => handle
                .with_current(|f| format!("{}", f))
                .map_err(Into::into),
            Self::Plain(handle) => handle
                .with_current(|f| format!("{}", f))
                .map_err(Into::into),
        }
    }
}

impl fmt::Debug for LevelHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(handle) => handle
                .with_current(|c| {
                    f.debug_struct("LevelHandle")
                        .field("current", &format_args!("{}", c))
                        .finish()
                })
                .unwrap_or_else(|e| {
                    f.debug_struct("LevelHandle")
                        .field("current", &format_args!("{}", e))
                        .finish()
                }),
            Self::Plain(handle) => handle
                .with_current(|c| {
                    f.debug_struct("LevelHandle")
                        .field("current", &format_args!("{}", c))
                        .finish()
                })
                .unwrap_or_else(|e| {
                    f.debug_struct("LevelHandle")
                        .field("current", &format_args!("{}", e))
                        .finish()
                }),
        }
    }
}
