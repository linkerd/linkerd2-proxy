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
    filter: String,
    format: String,
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
            filter: std::env::var(ENV_LOG_LEVEL)
                .ok()
                .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_string()),
            format: std::env::var(ENV_LOG_FORMAT)
                .ok()
                .unwrap_or_else(|| DEFAULT_LOG_FORMAT.to_string()),
            access_log: Self::access_log_format(),
            start_time: Some(start_time),
            is_test: false,
        }
    }

    fn for_test(filter: String, format: String) -> Self {
        Self {
            filter,
            format,
            start_time: None,
            access_log: Self::access_log_format(),
            is_test: true,
        }
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
        if self.filter.trim().eq_ignore_ascii_case("off") {
            return Ok(Handle {
                level: None,

                // logging is disabled, but log streaming might still be enabled later
                #[cfg(feature = "stream")]
                stream: {
                    let (handle, layer) = stream::StreamHandle::new();
                    tracing::dispatcher::set_global_default(
                        tracing_subscriber::registry().with(None).with(layer).into(),
                    )?;
                    handle
                },
            });
        }

        let (dispatch, handle) = self.build();
        // Set the default subscriber.
        tracing::dispatcher::set_global_default(dispatch)?;

        // Set up log compatibility.
        init_log_compat()?;

        Ok(handle)
    }

    /// Builds a tracing subscriber dispatcher and a handle that can control
    /// logging behavior at runtime (e.g., from an admin server).
    ///
    /// The log dispatcher handles:
    ///
    /// - process diagnostic logging to stdout;
    /// - optional access logging to stderr;
    /// - if the `stream` feature is enabled, on-demand log streaming via the
    ///   returned `Handle`
    pub fn build(self) -> (Dispatch, Handle) {
        let registry = tracing_subscriber::registry();

        // Build the default stdout logger.
        let (registry, level) = {
            // Make a formatted logging layer configured to write to stdout.
            let stdout = if self.format.eq_ignore_ascii_case("json") {
                self.mk_json()
            } else {
                self.mk_plain()
            };

            // Parse the initial filter. If the filter includes invalid
            // directives, an error is printed sto stderr.
            let filter = level::filter_builder().parse_lossy(self.filter);

            // Make the level dynamic and register the layer.
            let (layer, level) = reload::Layer::new(stdout.with_filter(filter));
            (registry.with(Some(layer)), level)
        };

        // Log streaming (via the admin API) is currently feature-gated. When it
        // is enabled, the admin handle can use the stream handle to register
        // new subscribers dynamically.
        #[cfg(feature = "stream")]
        let (registry, stream) = {
            let (handle, layer) = stream::StreamHandle::new();
            (registry.with(layer), handle)
        };

        // Access logging is optionally enabled process-wide.
        let registry = registry.with(self.access_log.map(access_log::build));

        // The handle controls the logging system at runtime.
        let handle = Handle {
            level: Some(level::Handle::new(level)),
            #[cfg(feature = "stream")]
            stream,
        };

        (registry.into(), handle)
    }
}

// TODO(eliza): Simplify `tracing-subscriber::reload::Handle` type parameters.
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
    pub fn into_stream(self) -> stream::StreamHandle<LogStack> {
        self.stream
    }
}
