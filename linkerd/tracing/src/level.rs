use linkerd_error::Error;
use tracing::trace;
use tracing_subscriber::{reload, EnvFilter, Registry};

#[derive(Clone)]
pub struct Handle(reload::Handle<EnvFilter, Registry>);

impl Handle {
    pub(crate) fn new(handle: reload::Handle<EnvFilter, Registry>) -> Self {
        Self(handle)
    }

    pub fn set_from(&self, bytes: impl AsRef<[u8]>) -> Result<(), String> {
        let body = std::str::from_utf8(bytes.as_ref()).map_err(|e| format!("{}", e))?;
        trace!(request.body = ?body);
        self.set_level(body).map_err(|e| format!("{}", e))
    }

    pub fn set_level(&self, level: impl AsRef<str>) -> Result<(), Error> {
        let level = level.as_ref();
        let filter = level.parse::<EnvFilter>()?;
        self.0.reload(filter)?;
        tracing::info!(%level, "set new log level");
        Ok(())
    }

    pub fn current(&self) -> Result<String, Error> {
        self.0
            .with_current(|f| format!("{}", f))
            .map_err(Into::into)
    }
}
