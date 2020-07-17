use linkerd2_error::Error;
use std::{env, fmt, str, time::Instant};
use tokio_timer::clock;
use tracing::Dispatch;
use tracing_subscriber::{
    fmt::{format, Formatter},
    reload, EnvFilter, FmtSubscriber,
};

const ENV_LOG_LEVEL: &str = "LINKERD2_PROXY_LOG";
const ENV_LOG_FORMAT: &str = "LINKERD2_PROXY_LOG_FORMAT";

const DEFAULT_LOG_LEVEL: &str = "warn,linkerd2_proxy=info";
const DEFAULT_LOG_FORMAT: &str = "PLAIN";

type JsonFormatter = Formatter<format::JsonFields, format::Format<format::Json, Uptime>>;
type PlainFormatter = Formatter<format::DefaultFields, format::Format<format::Full, Uptime>>;

#[derive(Clone)]
pub enum LevelHandle {
    Json(reload::Handle<EnvFilter, JsonFormatter>),
    Plain(reload::Handle<EnvFilter, PlainFormatter>),
}

/// Initialize tracing and logging with the value of the `ENV_LOG`
/// environment variable as the verbosity-level filter.
pub fn init() -> Result<LevelHandle, Error> {
    let log_level = env::var(ENV_LOG_LEVEL).unwrap_or(DEFAULT_LOG_LEVEL.to_string());
    let log_format = env::var(ENV_LOG_FORMAT).unwrap_or(DEFAULT_LOG_FORMAT.to_string());

    let (dispatch, handle) = with_filter_and_format(log_level, log_format);

    // Set up log compatibility.
    init_log_compat()?;
    // Set the default subscriber.
    tracing::dispatcher::set_global_default(dispatch)?;
    Ok(handle)
}

pub fn init_log_compat() -> Result<(), Error> {
    tracing_log::LogTracer::init().map_err(Error::from)
}

pub fn with_filter_and_format(
    filter: impl AsRef<str>,
    format: impl AsRef<str>,
) -> (Dispatch, LevelHandle) {
    let filter = filter.as_ref();

    // Set up the subscriber
    let start_time = clock::now();

    let builder = FmtSubscriber::builder()
        .with_timer(Uptime { start_time })
        .with_env_filter(filter);

    match format.as_ref().to_uppercase().as_ref() {
        "JSON" => {
            let builder = builder.json().with_span_list(true).with_filter_reloading();
            let handle = LevelHandle::Json(builder.reload_handle());
            let dispatch = Dispatch::new(builder.finish());
            (dispatch, handle)
        }
        "PLAIN" | _ => {
            let builder = builder.with_ansi(cfg!(test)).with_filter_reloading();
            let handle = LevelHandle::Plain(builder.reload_handle());
            let dispatch = Dispatch::new(builder.finish());
            (dispatch, handle)
        }
    }
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

impl LevelHandle {
    /// Returns a new `LevelHandle` without a corresponding filter.
    ///
    /// This will do nothing, but is required for admin endpoint tests which
    /// do not exercise the `proxy-log-level` endpoint.
    pub fn dangling() -> Self {
        let (_, handle) = with_filter_and_format("", "");
        handle
    }

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
