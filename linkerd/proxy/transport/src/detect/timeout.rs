use super::Detect;
use bytes::BytesMut;
use futures::prelude::*;
use linkerd_error::Error;
use linkerd_io as io;
use tokio::time;

#[derive(Copy, Clone, Debug)]
pub struct DetectTimeout<D> {
    inner: D,
    timeout: time::Duration,
}

#[derive(Debug)]
pub struct DetectTimeoutError {
    bytes: usize,
    elapsed: time::Duration,
}

// === impl DetectTimeout ===

impl<D> DetectTimeout<D> {
    pub fn new(timeout: time::Duration, inner: D) -> Self {
        Self { inner, timeout }
    }
}

#[async_trait::async_trait]
impl<D> Detect for DetectTimeout<D>
where
    D: Detect,
    D::Protocol: std::fmt::Debug,
{
    type Protocol = D::Protocol;

    async fn detect<I: io::AsyncRead + Send + Unpin + 'static>(
        &self,
        io: &mut I,
        buf: &mut BytesMut,
    ) -> Result<Option<Self::Protocol>, Error> {
        let t0 = time::Instant::now();
        let timeout = time::sleep(self.timeout);
        let detect = self.inner.detect(io, buf);
        futures::select_biased! {
            res = detect.fuse() => res,
            _ = timeout.fuse() => {
                let bytes = buf.len();
                let elapsed = time::Instant::now() - t0;
                Err(DetectTimeoutError { bytes, elapsed }.into())
            }
        }
    }
}

// === impl DetectTimeout ===

impl std::fmt::Display for DetectTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Protocol detection timeout after: {}B after {:?}",
            self.bytes, self.elapsed
        )
    }
}

impl std::error::Error for DetectTimeoutError {}
