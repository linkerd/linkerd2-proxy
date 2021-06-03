use linkerd_access_log::tracing::AccessLogWriter;
use std::{path::PathBuf, sync::Arc};
pub use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::filter::Directive;

const ENV_ACCESS_LOG: &str = "LINKERD2_PROXY_ACCESS_LOG";

#[derive(Clone, Debug)]
pub struct Guard(Arc<WorkerGuard>);

pub type Writer = AccessLogWriter<NonBlocking>;

pub(super) fn build() -> Option<(Writer, Guard, Directive)> {
    // Create the access log file, or open it in append-only mode if
    // it already exists.
    let file = {
        let path = {
            let p = std::env::var(ENV_ACCESS_LOG).ok()?;
            p.parse::<PathBuf>()
                .map_err(|e| eprintln!("{} is not a valid path: {}", p, e))
                .ok()?
        };

        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|e| eprintln!("failed to open file {}: {}", path.display(), e,))
            .ok()?
    };

    // If we successfully created or opened the access log file,
    // build the access log layer.
    eprintln!("writing access log to {:?}", file);
    let (non_blocking, guard) = tracing_appender::non_blocking(file);
    let writer = AccessLogWriter::new().with_writer(non_blocking);

    // Also, ensure that the `tracing` filter configuration will
    // always enable the access log spans.
    let directive = "access_log=info"
        .parse()
        .expect("hard-coded filter directive must parse");

    Some((writer, Guard(guard.into()), directive))
}
