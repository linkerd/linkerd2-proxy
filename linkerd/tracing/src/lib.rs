#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod access_log;
pub mod level;
#[cfg(feature = "stream")]
pub mod stream;
pub mod test;
mod uptime;

use self::uptime::Uptime;
use linkerd_error::Error;
use std::str;
use tokio::time::Instant;
use tracing::Dispatch;
use tracing_subscriber::{
    filter::LevelFilter, fmt::format, prelude::*, registry::LookupSpan, reload, Layer,
};
#[cfg(feature = "stream")]
use tracing_subscriber::{layer::Layered, Registry};

pub use tracing::Subscriber;
pub use tracing_subscriber::{registry, EnvFilter};

const ENV_LOG_LEVEL: &str = "LINKERD2_PROXY_LOG";
const ENV_LOG_FORMAT: &str = "LINKERD2_PROXY_LOG_FORMAT";
const ENV_ACCESS_LOG: &str = "LINKERD2_PROXY_ACCESS_LOG";

const DEFAULT_LOG_LEVEL: &str = "warn,linkerd=info";
const DEFAULT_LOG_FORMAT: &str = "PLAIN";

#[derive(Debug, Default)]
#[must_use]
pub struct Settings {
    filter: Option<String>,
    format: Option<String>,
    start_time: Option<Instant>,
    access_log: Option<access_log::Format>,
    is_test: bool,
}

#[derive(Clone)]
pub struct Handle {
    level: Option<level::Handle>,
    #[cfg(feature = "stream")]
    stream: stream::StreamHandle<LogStack>,
}

#[inline]
pub(crate) fn update_max_level() {
    use tracing_log::{log, AsLog};
    log::set_max_level(LevelFilter::current().as_log());
}

pub fn init_log_compat() -> Result<(), Error> {
    tracing_log::LogTracer::init()?;
    // Set the initial max `log` level based on the subscriber settings.
    update_max_level();
    Ok(())
}

// === impl Settings ===

impl Settings {
    pub fn from_env(start_time: Instant) -> Self {
        Self {
            filter: std::env::var(ENV_LOG_LEVEL).ok(),
            format: std::env::var(ENV_LOG_FORMAT).ok(),
            access_log: Self::access_log_format(),
            start_time: Some(start_time),
            is_test: false,
        }
    }

    fn for_test(filter: String, format: String) -> Self {
        Self {
            filter: Some(filter),
            format: Some(format),
            start_time: None,
            access_log: Self::access_log_format(),
            is_test: true,
        }
    }

    fn format(&self) -> String {
        self.format
            .as_deref()
            .unwrap_or(DEFAULT_LOG_FORMAT)
            .to_uppercase()
    }

    fn access_log_format() -> Option<access_log::Format> {
        let env = std::env::var(ENV_ACCESS_LOG).ok()?;
        match env.parse() {
            Ok(format) => Some(format),
            Err(err) => {
                eprintln!("Invalid {}={:?}: {}", ENV_ACCESS_LOG, env, err);
                None
            }
        }
    }

    fn timer(&self) -> Uptime {
        self.start_time
            .map(Uptime::starting_at)
            .unwrap_or_else(Uptime::starting_now)
    }

    fn mk_json<S>(&self) -> Box<dyn Layer<S> + Send + Sync + 'static>
    where
        S: Subscriber + for<'span> LookupSpan<'span>,
        S: Send + Sync,
    {
        let fmt = tracing_subscriber::fmt::format()
            .with_timer(self.timer())
            .with_thread_ids(!self.is_test)
            // Configure the formatter to output JSON logs.
            .json()
            // Output the current span context as a JSON list.
            .with_span_list(true)
            // Don't output a field for the current span, since this
            // would duplicate information already in the span list.
            .with_current_span(false);

        let fmt = tracing_subscriber::fmt::layer()
            // Use the JSON event formatter.
            .event_format(fmt)
            // Since we're using the JSON event formatter, we must also
            // use the JSON field formatter.
            .fmt_fields(format::JsonFields::default());

        if self.is_test {
            Box::new(fmt.with_test_writer())
        } else {
            Box::new(fmt)
        }
    }

    fn mk_plain<S>(&self) -> Box<dyn Layer<S> + Send + Sync + 'static>
    where
        S: Subscriber + for<'span> LookupSpan<'span>,
        S: Send + Sync,
    {
        let fmt = tracing_subscriber::fmt::format()
            .with_timer(self.timer())
            .with_thread_ids(!self.is_test);
        let fmt = tracing_subscriber::fmt::layer().event_format(fmt);
        if self.is_test {
            Box::new(fmt.with_test_writer())
        } else {
            Box::new(fmt)
        }
    }

    /// Initialize tracing and logging with the value of the `ENV_LOG`
    /// environment variable as the verbosity-level filter.
    pub fn init(self) -> Result<Handle, Error> {
        let (dispatch, handle) = match self.filter.as_deref() {
            Some(filter) if filter.trim().eq_ignore_ascii_case("off") => {
                // logging is disabled, but log streaming might still be enabled later
                #[cfg(feature = "stream")]
                let stream = {
                    let (handle, layer) = stream::StreamHandle::new();
                    tracing::dispatcher::set_global_default(
                        tracing_subscriber::registry().with(None).with(layer).into(),
                    )?;
                    handle
                };

                return Ok(Handle {
                    level: None,

                    #[cfg(feature = "stream")]
                    stream,
                });
            }
            _ => self.build(),
        };

        // Set the default subscriber.
        tracing::dispatcher::set_global_default(dispatch)?;

        // Set up log compatibility.
        init_log_compat()?;

        Ok(handle)
    }

    pub fn build(self) -> (Dispatch, Handle) {
        let log_level = self.filter.as_deref().unwrap_or(DEFAULT_LOG_LEVEL);

        let filter = level::filter_builder()
            // When parsing the initial filter configuration from the
            // environment variable, use `parse_lossy` to skip any invalid
            // filter directives and print an error.
            .parse_lossy(log_level);

        // If access logging is enabled, build the access log layer.
        let access_log = self.access_log.map(access_log::build);

        let logger = match self.format().as_ref() {
            "JSON" => self.mk_json(),
            _ => self.mk_plain(),
        };
        let logger = logger.with_filter(filter);
        let (logger, level) = reload::Layer::new(logger);
        let level = level::Handle::new(level);

        #[cfg(feature = "stream")]
        let (stream, stream_layer) = stream::StreamHandle::new();

        let handle = Handle {
            level: Some(level),
            #[cfg(feature = "stream")]
            stream,
        };

        let registry = tracing_subscriber::registry().with(Some(logger));
        #[cfg(feature = "stream")]
        let registry = registry.with(stream_layer);

        let dispatch = registry.with(access_log).into();

        (dispatch, handle)
    }
}

// TODO(eliza): when `tracing-subscriber` fixes the `reload::Handle` type
// parameter mess, this is going to get a lot awful...
#[cfg(feature = "stream")]
type LogStack = Layered<Option<reload::Layer<level::Inner, Registry>>, Registry>;

// === impl Handle ===

impl Handle {
    /// Returns a new `handle` with tracing disabled.
    pub fn disabled() -> Self {
        Self {
            level: None,
            #[cfg(feature = "stream")]
            stream: stream::StreamHandle::new().0,
        }
    }

    pub fn level(&self) -> Option<&level::Handle> {
        self.level.as_ref()
    }

    #[cfg(feature = "stream")]
    pub fn stream(&self) -> &stream::StreamHandle<LogStack> {
        &self.stream
    }
}
