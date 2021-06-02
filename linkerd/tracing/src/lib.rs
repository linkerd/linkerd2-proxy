#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

pub mod level;
pub mod test;
mod uptime;

use self::uptime::Uptime;
use linkerd_error::Error;
use std::{env, str};
pub use tokio_trace::tasks::TaskList;
use tokio_trace::tasks::TasksLayer;
use tracing::Dispatch;
use tracing_subscriber::{fmt::format, prelude::*};

const ENV_LOG_LEVEL: &str = "LINKERD2_PROXY_LOG";
const ENV_LOG_FORMAT: &str = "LINKERD2_PROXY_LOG_FORMAT";

const DEFAULT_LOG_LEVEL: &str = "warn,linkerd=info";
const DEFAULT_LOG_FORMAT: &str = "PLAIN";

/// Initialize tracing and logging with the value of the `ENV_LOG`
/// environment variable as the verbosity-level filter.
pub fn init() -> Result<Handle, Error> {
    let log_level = env::var(ENV_LOG_LEVEL).unwrap_or_else(|_| DEFAULT_LOG_LEVEL.to_string());
    if let "OFF" = log_level.to_uppercase().trim() {
        return Ok(Handle(Inner::Disabled));
    }

    let log_format = env::var(ENV_LOG_FORMAT).unwrap_or_else(|_| DEFAULT_LOG_FORMAT.to_string());
    let (dispatch, handle) = Settings::default()
        .filter(log_level)
        .format(log_format)
        .build();

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

#[derive(Debug, Default)]
pub struct Settings {
    filter: Option<String>,
    format: Option<String>,
    test: bool,
}

impl Settings {
    pub fn from_env() -> Self {
        let mut settings = Settings::default();
        if let Ok(filter) = env::var(ENV_LOG_LEVEL) {
            settings = settings.filter(filter);
        }
        if let Ok(format) = env::var(ENV_LOG_FORMAT) {
            settings = settings.format(format);
        }
        settings
    }

    pub fn filter(self, filter: impl Into<String>) -> Self {
        Self {
            filter: Some(filter.into()),
            ..self
        }
    }

    pub fn format(self, format: impl Into<String>) -> Self {
        Self {
            format: Some(format.into()),
            ..self
        }
    }

    pub fn test(self, test: bool) -> Self {
        Self { test, ..self }
    }

    pub fn build(self) -> (Dispatch, Handle) {
        let filter = self.filter.unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_string());
        let format = self
            .format
            .unwrap_or_else(|| DEFAULT_LOG_FORMAT.to_string());

        // Set up the subscriber
        let fmt = tracing_subscriber::fmt::format()
            .with_timer(Uptime::starting_now())
            .with_thread_ids(!self.test);
        let filter = tracing_subscriber::EnvFilter::new(filter);
        let (filter, level) = tracing_subscriber::reload::Layer::new(filter);
        let level = level::Handle::new(level);
        let registry = tracing_subscriber::registry().with(filter);

        let (dispatch, tasks) = match format.to_uppercase().as_ref() {
            "JSON" => {
                let (tasks, tasks_layer) = TasksLayer::<format::JsonFields>::new();
                let fmt = fmt
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
                let registry = registry.with(tasks_layer);
                let dispatch = if self.test {
                    registry.with(fmt.with_test_writer()).into()
                } else {
                    registry.with(fmt).into()
                };
                (dispatch, tasks)
            }
            _ => {
                let (tasks, tasks_layer) = TasksLayer::<format::DefaultFields>::new();
                let registry = registry.with(tasks_layer);
                let fmt = tracing_subscriber::fmt::layer().event_format(fmt);
                let dispatch = if self.test {
                    registry.with(fmt.with_test_writer()).into()
                } else {
                    registry.with(fmt).into()
                };
                (dispatch, tasks)
            }
        };

        (dispatch, Handle(Inner::Enabled { level, tasks }))
    }
}

#[derive(Clone)]
pub struct Handle(Inner);

#[derive(Clone)]
enum Inner {
    Disabled,
    Enabled {
        level: level::Handle,
        tasks: TaskList,
    },
}

// === impl Handle ===

impl Handle {
    /// Returns a new `handle` with tracing disabled.
    pub fn disabled() -> Self {
        Self(Inner::Disabled)
    }

    pub fn level(&self) -> Option<&level::Handle> {
        match self.0 {
            Inner::Enabled { ref level, .. } => Some(level),
            Inner::Disabled => None,
        }
    }

    pub fn tasks(&self) -> Option<&TaskList> {
        match self.0 {
            Inner::Enabled { ref tasks, .. } => Some(tasks),
            Inner::Disabled => None,
        }
    }
}
