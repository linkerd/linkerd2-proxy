use linkerd_error::Error;
use tracing::trace;
use tracing_subscriber::{
    filter::{self, EnvFilter, Filtered, LevelFilter},
    reload, Layer, Registry,
};

#[derive(Clone)]
pub struct Handle(reload::Handle<Inner, Registry>);

pub(crate) type Inner =
    Filtered<Box<dyn Layer<Registry> + Send + Sync + 'static>, EnvFilter, Registry>;

/// Returns an `EnvFilter` builder with the configuration used for parsing new
/// filter strings.
pub(crate) fn filter_builder() -> filter::Builder {
    EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into())
        // Disable regular expression matching for `fmt::Debug` fields, use
        // exact string matching instead.
        .with_regex(false)
}

impl Handle {
    pub(crate) fn new(handle: reload::Handle<Inner, Registry>) -> Self {
        Self(handle)
    }

    pub fn set_from(&self, bytes: impl AsRef<[u8]>) -> Result<(), String> {
        let body = std::str::from_utf8(bytes.as_ref()).map_err(|e| format!("{}", e))?;
        trace!(request.body = ?body);
        self.set_level(body).map_err(|e| format!("{}", e))
    }

    pub fn set_level(&self, level: impl AsRef<str>) -> Result<(), Error> {
        let level = level.as_ref();
        let filter = filter_builder().parse(level)?;
        self.0.modify(|layer| {
            *layer.filter_mut() = filter;
        })?;
        tracing::info!(%level, "set new log level");
        Ok(())
    }

    pub fn current(&self) -> Result<String, Error> {
        self.0
            .with_current(|f| format!("{}", f.filter()))
            .map_err(Into::into)
    }
}
