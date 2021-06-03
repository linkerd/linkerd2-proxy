#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

mod access_log;
pub mod level;
pub mod test;
mod uptime;

use self::uptime::Uptime;
use linkerd_error::Error;
pub use tokio_trace::tasks::TaskList;
use tokio_trace::tasks::TasksLayer;
use tracing::Dispatch;
use tracing_subscriber::{
    fmt::format::{self, DefaultFields},
    layer::Layered,
    prelude::*,
    reload, EnvFilter,
};

type Registry = Layered<
    access_log::Writer,
    Layered<reload::Layer<EnvFilter, tracing_subscriber::Registry>, tracing_subscriber::Registry>,
>;

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
pub struct Handle(Inner);

#[derive(Clone)]
enum Inner {
    Disabled,
    Enabled {
        level: level::Handle,
        tasks: TaskList,
        guard: access_log::Guard,
    },
}

/// Initialize tracing and logging with the value of the `ENV_LOG`
/// environment variable as the verbosity-level filter.
pub fn init() -> Result<Handle, Error> {
    let (dispatch, handle) = match Settings::from_env() {
        Some(s) => s.build(),
        None => return Ok(Handle(Inner::Disabled)),
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

    fn mk_registry(&self) -> (Registry, level::Handle, access_log::Guard) {
        let filter = {
            let f = self.filter.as_deref().unwrap_or(DEFAULT_LOG_LEVEL);
            EnvFilter::new(f)
        };

        let (access_log, guard, directive) =
            access_log::build().expect("XXX FIXME handle case when access log isn't used");
        let filter = filter.add_directive(directive);

        let (filter, level) = reload::Layer::new(filter);
        let reg = tracing_subscriber::registry().with(filter).with(access_log);
        (reg, level::Handle::new(level), guard)
    }

    fn mk_json(&self, registry: Registry) -> (Dispatch, TaskList) {
        let (tasks, tasks_layer) = TasksLayer::<format::JsonFields>::new();
        let registry = registry.with(tasks_layer);

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

        let dispatch = if self.is_test {
            registry.with(fmt.with_test_writer()).into()
        } else {
            registry.with(fmt).into()
        };

        (dispatch, tasks)
    }

    fn mk_plain(&self, registry: Registry) -> (Dispatch, TaskList) {
        let (tasks, tasks_layer) = TasksLayer::<DefaultFields>::new();
        let registry = registry.with(tasks_layer);

        let fmt = tracing_subscriber::fmt::format()
            .with_timer(Uptime::starting_now())
            .with_thread_ids(!self.is_test);
        let fmt = tracing_subscriber::fmt::layer().event_format(fmt);
        let dispatch = if self.is_test {
            registry.with(fmt.with_test_writer()).into()
        } else {
            registry.with(fmt).into()
        };

        (dispatch, tasks)
    }

    pub fn build(self) -> (Dispatch, Handle) {
        let (registry, level, guard) = self.mk_registry();

        let (dispatch, tasks) = match self.format().as_ref() {
            "JSON" => self.mk_json(registry),
            _ => self.mk_plain(registry),
        };

        let handle = Handle(Inner::Enabled {
            level,
            tasks,
            guard,
        });

        (dispatch, handle)
    }
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
