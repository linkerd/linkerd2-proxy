#![deny(warnings, rust_2018_idioms)]

mod level;
mod tasks;
mod uptime;

use self::uptime::Uptime;
use linkerd2_error::Error;
use std::{env, str};
use tokio_trace::tasks::TasksLayer;
use tracing::Dispatch;
use tracing_subscriber::{
    fmt::{format, Layer as FmtLayer},
    layer::Layered,
    prelude::*,
};

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

/// Initialize tracing and logging with the value of the `ENV_LOG`
/// environment variable as the verbosity-level filter.
pub fn init() -> Result<Handle, Error> {
    let log_level = env::var(ENV_LOG_LEVEL).unwrap_or(DEFAULT_LOG_LEVEL.to_string());
    if let "OFF" = log_level.to_uppercase().trim() {
        return Ok(Handle(Inner::Disabled));
    }

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
) -> (Dispatch, Handle) {
    let filter = filter.as_ref();

    // Set up the subscriber
    let filter = tracing_subscriber::EnvFilter::new(filter);
    let formatter = tracing_subscriber::fmt::format()
        .with_timer(Uptime::starting_now())
        .with_thread_ids(true);

    let (dispatch, level, tasks) = match format.as_ref().to_uppercase().as_ref() {
        "JSON" => {
            let (tasks, tasks_layer) = TasksLayer::<format::JsonFields>::new();
            let (filter, level) = tracing_subscriber::reload::Layer::new(filter);
            let dispatch = tracing_subscriber::registry()
                .with(tasks_layer)
                .with(
                    tracing_subscriber::fmt::layer()
                        .event_format(formatter)
                        .json()
                        .with_span_list(true),
                )
                .with(filter)
                .into();
            let handle = level::Handle::Json(level);
            (dispatch, handle, tasks)
        }
        _ => {
            let (tasks, tasks_layer) = TasksLayer::<format::DefaultFields>::new();
            let (filter, level) = tracing_subscriber::reload::Layer::new(filter);
            let dispatch = tracing_subscriber::registry()
                .with(tasks_layer)
                .with(tracing_subscriber::fmt::layer().event_format(formatter))
                .with(filter)
                .into();
            let handle = level::Handle::Plain(level);
            (dispatch, handle, tasks)
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
