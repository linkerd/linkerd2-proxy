use deflate::{write::GzEncoder, CompressionOptions};
use hyper::Body;
use std::io::Write;
use tracing::trace;

use super::FmtMetrics;

/// Serve Prometheues metrics.
#[derive(Debug, Clone)]
pub struct Serve<M> {
    metrics: M,
}

// ===== impl Serve =====

impl<M> Serve<M> {
    pub fn new(metrics: M) -> Self {
        Self { metrics }
    }

    fn is_gzip<B>(req: &http::Request<B>) -> bool {
        req.headers()
            .get_all(http::header::ACCEPT_ENCODING)
            .iter()
            .any(|value| {
                value
                    .to_str()
                    .ok()
                    .map(|value| value.contains("gzip"))
                    .unwrap_or(false)
            })
    }
}

impl<M: FmtMetrics> Serve<M> {
    pub fn serve<B>(&self, req: http::Request<B>) -> std::io::Result<http::Response<Body>> {
        if Self::is_gzip(&req) {
            trace!("gzipping metrics");
            let mut writer = GzEncoder::new(Vec::<u8>::new(), CompressionOptions::fast());
            write!(&mut writer, "{}", self.metrics.as_display())?;
            Ok(http::Response::builder()
                .header(http::header::CONTENT_ENCODING, "gzip")
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body(writer.finish()?.into())
                .expect("Response must be valid"))
        } else {
            let mut writer = Vec::<u8>::new();
            write!(&mut writer, "{}", self.metrics.as_display())?;
            Ok(http::Response::builder()
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body(Body::from(writer))
                .expect("Response must be valid"))
        }
    }
}
