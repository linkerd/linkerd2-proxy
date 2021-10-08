#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

pub mod access_log;
pub mod level;
pub mod test;
mod uptime;

use self::uptime::Uptime;
use linkerd_error::Error;
use std::str;
use tracing::{Dispatch, Subscriber};
use tracing_subscriber::{
    filter::{EnvFilter, FilterFn},
    fmt::format,
    prelude::*,
    registry::LookupSpan,
    reload, Layer,
};

const ENV_LOG_LEVEL: &str = "LINKERD2_PROXY_LOG";
const ENV_LOG_FORMAT: &str = "LINKERD2_PROXY_LOG_FORMAT";

const DEFAULT_LOG_LEVEL: &str = "warn,linkerd=info";
const DEFAULT_LOG_FORMAT: &str = "PLAIN";

#[derive(Debug, Default)]
pub struct Settings {
    filter: Option<String>,
    format: Option<String>,
    is_test: bool,
}

#[derive(Clone)]
pub struct Handle(Option<Inner>);

#[derive(Clone)]
struct Inner {
    level: level::Handle,
    guard: Option<access_log::Guard>,
}

/// Initialize tracing and logging with the value of the `ENV_LOG`
/// environment variable as the verbosity-level filter.
pub fn init() -> Result<Handle, Error> {
    let (dispatch, handle) = match Settings::from_env() {
        Some(s) => s.build(),
        None => return Ok(Handle(None)),
    };

    // Set the default subscriber.
    tracing::dispatcher::set_global_default(dispatch)?;

    // Set up log compatibility.
    init_log_compat()?;

    Ok(handle)
}

#[inline]
pub(crate) fn update_max_level() {
    use tracing::level_filters::LevelFilter;
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
    pub fn from_env() -> Option<Self> {
        let filter = std::env::var(ENV_LOG_LEVEL).ok();
        if let Some(level) = filter.as_ref() {
            if level.to_uppercase().trim() == "OFF" {
                return None;
            }
        }

        Some(Self {
            filter,
            format: std::env::var(ENV_LOG_FORMAT).ok(),
            is_test: false,
        })
    }

    fn for_test(filter: String, format: String) -> Self {
        Self {
            filter: Some(filter),
            format: Some(format),
            is_test: true,
        }
    }

    fn format(&self) -> String {
        self.format
            .as_deref()
            .unwrap_or(DEFAULT_LOG_FORMAT)
            .to_uppercase()
    }

    fn mk_json<S>(&self) -> Box<dyn Layer<S> + Send + Sync + 'static>
    where
        S: Subscriber + for<'span> LookupSpan<'span>,
        S: Send + Sync,
    {
        let fmt = tracing_subscriber::fmt::format()
            .with_timer(Uptime::starting_now())
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
            .with_timer(Uptime::starting_now())
            .with_thread_ids(!self.is_test);
        let fmt = tracing_subscriber::fmt::layer().event_format(fmt);
        if self.is_test {
            Box::new(fmt.with_test_writer())
        } else {
            Box::new(fmt)
        }
    }

    pub fn build(self) -> (Dispatch, Handle) {
        let log_level = self.filter.as_deref().unwrap_or(DEFAULT_LOG_LEVEL);
        let (filter, level) = reload::Layer::new(EnvFilter::new(log_level));
        let level = level::Handle::new(level);

        let logger = match self.format().as_ref() {
            "JSON" => self.mk_json(),
            _ => self.mk_plain(),
        };

        let dispatch = tracing_subscriber::registry()
            .with(filter)
            .with(logger)
            .into();

        (dispatch, Handle(Some(level)))
    }
}
// === impl Handle ===

impl Handle {
    /// Returns a new `handle` with tracing disabled.
    pub fn disabled() -> Self {
        Self(None)
    }

    pub fn level(&self) -> Option<&level::Handle> {
        self.0.as_ref().map(|inner| &inner.level)
    }
}
