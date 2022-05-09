#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
#![forbid(unsafe_code)]

pub mod access_log;
pub mod level;
pub mod test;
mod uptime;

use self::uptime::Uptime;
use linkerd_error::Error;
use std::str;
use tokio::time::Instant;
use tracing::{Dispatch, Subscriber};
use tracing_subscriber::{
    filter::{FilterFn, LevelFilter},
    fmt::format,
    prelude::*,
    registry::LookupSpan,
    reload, Layer,
};

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
pub struct Handle(Option<level::Handle>);

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
    pub fn from_env() -> Self {
        Self {
            filter: std::env::var(ENV_LOG_LEVEL).ok(),
            format: std::env::var(ENV_LOG_FORMAT).ok(),
            access_log: Self::access_log_format(),
            start_time: None,
            is_test: false,
        }
    }

    pub fn with_start_time(self, start: Instant) -> Self {
        Self {
            start_time: Some(start),
            ..self
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
            Some(filter) if filter.trim().eq_ignore_ascii_case("off") => return Ok(Handle(None)),
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

        let mut filter = level::filter_builder()
            // When parsing the initial filter configuration from the
            // environment variable, use `parse_lossy` to skip any invalid
            // filter directives and print an error.
            .parse_lossy(log_level);

        // If access logging is enabled, build the access log layer.
        let access_log = if let Some(format) = self.access_log {
            let (access_log, directive) = access_log::build(format);
            filter = filter.add_directive(directive);
            Some(access_log)
        } else {
            None
        };

        let (filter, level) = reload::Layer::new(filter);
        let level = level::Handle::new(level);

        let logger = match self.format().as_ref() {
            "JSON" => self.mk_json(),
            _ => self.mk_plain(),
        };
        let logger = logger.with_filter(FilterFn::new(|meta| {
            !meta.target().starts_with(access_log::TRACE_TARGET)
        }));

        let handle = Handle(Some(level));

        let dispatch = tracing_subscriber::registry()
            .with(filter)
            .with(access_log)
            .with(logger)
            .into();

        (dispatch, handle)
    }
}
// === impl Handle ===

impl Handle {
    /// Returns a new `handle` with tracing disabled.
    pub fn disabled() -> Self {
        Self(None)
    }

    pub fn level(&self) -> Option<&level::Handle> {
        self.0.as_ref()
    }
}
