<!-- markdownlint-disable-file MD041 -->
<!-- Appended to PROMPT.md under "## Repository extension (supplied)". -->

## Critical paths in this repo

- **TLS.** `rustls`, `tokio-rustls`, `rustls-webpki`, and `aws-lc-*`, which
  `linkerd/rustls` and `linkerd/meshtls` use for mTLS, including the FIPS build.
- **HTTP and gRPC.** `hyper`, `h2`, `http`, `tower`, `tonic`, and `prost`.
- **Control-plane API.** `linkerd2-proxy-api`.
- **Runtime.** `tokio`, built with `--cfg tokio_unstable`.
- **DNS.** `hickory-resolver`.
- **Allocator.** `tikv-jemallocator`.
- **Build and test base.** The Rust toolchain and `ghcr.io/linkerd/dev`.
