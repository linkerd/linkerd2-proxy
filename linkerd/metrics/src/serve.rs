use bytes::Bytes;
use deflate::{write::GzEncoder, CompressionOptions};
use linkerd_http_box::BoxBody;
use std::io::Write;
use tracing::trace;

use super::FmtMetrics;

/// Serve Prometheues metrics.
#[derive(Debug, Clone)]
pub struct Serve<M> {
    metrics: M,
}

// === impl Serve ===

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
    pub fn serve<B>(&self, req: http::Request<B>) -> std::io::Result<http::Response<BoxBody>> {
        if Self::is_gzip(&req) {
            trace!("gzipping metrics");
            let mut writer = GzEncoder::new(Vec::<u8>::new(), CompressionOptions::fast());
            write!(&mut writer, "{}", self.metrics.as_display())?;
            Ok(http::Response::builder()
                .header(http::header::CONTENT_ENCODING, "gzip")
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body(BoxBody::new(http_body_util::Full::<Bytes>::from(
                    writer.finish().map(Bytes::from)?,
                )))
                .expect("Response must be valid"))
        } else {
            let mut writer = Vec::<u8>::new();
            write!(&mut writer, "{}", self.metrics.as_display())?;
            Ok(http::Response::builder()
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body(BoxBody::new(http_body_util::Full::<Bytes>::from(
                    Bytes::from(writer),
                )))
                .expect("Response must be valid"))
        }
    }
}
