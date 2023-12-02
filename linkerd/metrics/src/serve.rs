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
    pub fn serve<B>(&self, req: http::Request<B>) -> std::io::Result<http::Response<Body>> {
        if Self::is_gzip(&req) {
            trace!("gzipping metrics");
            let buf = {
                let mut writer = GzEncoder::new(Vec::<u8>::new(), CompressionOptions::fast());
                self.write(&mut writer)?;
                writer.finish()?
            };
            Ok(http::Response::builder()
                .header(http::header::CONTENT_ENCODING, "gzip")
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body(Body::from(buf))
                .expect("Response must be valid"))
        } else {
            let mut writer = Vec::<u8>::new();
            self.write(&mut writer)?;
            Ok(http::Response::builder()
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body(Body::from(writer))
                .expect("Response must be valid"))
        }
    }

    fn write(&self, f: &mut impl Write) -> std::io::Result<()> {
        write!(f, "{}", self.metrics.as_display())?;

        let prom = crate::prom::gather();
        let enc = crate::prom::TextEncoder::new();
        let txt = enc
            .encode_to_string(&prom)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        write!(f, "{}", txt)
    }
}
