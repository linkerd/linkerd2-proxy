#![deny(warnings, rust_2018_idioms)]

mod level;
mod tasks;
mod uptime;

use self::uptime::Uptime;
use linkerd2_error::Error;
use std::{env, str};
use tokio_trace::tasks::TasksLayer;
use tracing::Dispatch;
use tracing_subscriber::{fmt::format, prelude::*};

const ENV_LOG_LEVEL: &str = "LINKERD2_PROXY_LOG";
const ENV_LOG_FORMAT: &str = "LINKERD2_PROXY_LOG_FORMAT";

const DEFAULT_LOG_LEVEL: &str = "warn,linkerd2_proxy=info";
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

    // Set up log compatibility.
    init_log_compat()?;
    // Set the default subscriber.
    tracing::dispatcher::set_global_default(dispatch)?;

    Ok(handle)
}

pub fn init_log_compat() -> Result<(), Error> {
    tracing_log::LogTracer::init().map_err(Error::from)
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
                let fmt =
                    tracing_subscriber::fmt::layer().event_format(fmt.json().with_span_list(true));
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

        (
            dispatch,
            Handle(Inner::Enabled {
                level,
                tasks: tasks::Handle { tasks },
            }),
        )
    }
}

#[derive(Clone)]
pub struct Handle(Inner);

#[derive(Clone)]
enum Inner {
    Disabled,
    Enabled {
        level: level::Handle,
        tasks: tasks::Handle,
    },
}

// === impl Handle ===

impl Handle {
    /// Serve requests that controls the log level. The request is expected to be either a GET or PUT
    /// request. PUT requests must have a body that describes the new log level.
    pub async fn serve_level(
        &self,
        req: http::Request<hyper::Body>,
    ) -> Result<http::Response<hyper::Body>, Error> {
        match self.0 {
            Inner::Enabled { ref level, .. } => level.serve(req).await,
            Inner::Disabled => Ok(Self::not_found()),
        }
    }

    /// Serve requests for task dumps.
    pub async fn serve_tasks(
        &self,
        req: http::Request<hyper::Body>,
    ) -> Result<http::Response<hyper::Body>, Error> {
        match self.0 {
            Inner::Enabled { ref tasks, .. } => tasks.serve(req),
            Inner::Disabled => Ok(Self::not_found()),
        }
    }

    fn not_found() -> http::Response<hyper::Body> {
        http::Response::builder()
            .status(http::StatusCode::NOT_FOUND)
            .body(hyper::Body::empty())
            .expect("Response must be valid")
    }
}
