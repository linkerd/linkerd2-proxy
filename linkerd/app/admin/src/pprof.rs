use linkerd_app_core::Result;

#[derive(Copy, Clone, Debug)]
pub(crate) struct Pprof;

impl Pprof {
    pub async fn profile<B>(self, req: http::Request<B>) -> Result<http::Response<hyper::Body>> {
        use pprof::protos::Message;

        fn query_param<'r, B>(req: &'r http::Request<B>, name: &'static str) -> Option<&'r str> {
            let params = req.uri().path_and_query()?.query()?.split('&');
            params
                .filter_map(|p| p.strip_prefix(name)?.strip_prefix('='))
                .next()
        }

        let profile = {
            let duration = std::time::Duration::from_secs_f64(
                query_param(&req, "seconds")
                    .map(|s| s.parse::<f64>())
                    .transpose()?
                    .unwrap_or(30.0),
            );
            let frequency = query_param(&req, "frequency")
                .map(|s| s.parse::<i32>())
                .transpose()?
                // Go's default.
                .unwrap_or(100);
            tracing::info!(?duration, frequency, "Collecting");

            let guard = pprof::ProfilerGuard::new(frequency)?;
            tokio::time::sleep(duration).await;

            let report = guard.report().build()?;
            tracing::info!(
                ?duration,
                frequency,
                frames = report.data.len(),
                "Collected"
            );

            report.pprof()?
        };

        let pb_gz = {
            let mut gz = deflate::write::GzEncoder::new(
                Vec::<u8>::new(),
                deflate::CompressionOptions::fast(),
            );
            std::io::Write::write_all(&mut gz, &{
                let mut buf = Vec::new();
                profile.encode(&mut buf)?;
                buf
            })?;
            gz.finish()?
        };

        Ok(http::Response::builder()
            .header(http::header::CONTENT_TYPE, "application/octet-stream")
            .body(hyper::Body::from(pb_gz))
            .expect("response must be valid"))
    }
}
