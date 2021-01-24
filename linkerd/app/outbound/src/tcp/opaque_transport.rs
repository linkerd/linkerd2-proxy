use crate::target::Endpoint;
use linkerd_app_core::{
    dns, io, svc, tls,
    transport_header::{TransportHeader, PROTOCOL},
    Error,
};
use std::{
    future::Future,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};
use tracing::{debug, warn};

#[derive(Clone, Debug)]
pub struct OpaqueTransport<S> {
    inner: S,
}

impl<S> OpaqueTransport<S> {
    pub fn layer() -> impl svc::Layer<S, Service = Self> + Copy {
        svc::layer::mk(|inner| OpaqueTransport { inner })
    }

    fn expects_header<I: tls::HasNegotiatedProtocol>(io: &I) -> bool {
        if let Some(tls::NegotiatedProtocol(protocol)) = io.negotiated_protocol() {
            protocol == PROTOCOL
        } else {
            false
        }
    }
}

impl<S, P> svc::Service<Endpoint<P>> for OpaqueTransport<S>
where
    S: svc::Service<Endpoint<P>> + Send + 'static,
    S::Error: Into<Error>,
    S::Response: io::AsyncWrite + tls::HasNegotiatedProtocol + Send + Unpin,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut ep: Endpoint<P>) -> Self::Future {
        let orig_port = ep.addr.port();

        // Determine whether an opaque header should written on the socket.
        if let Some(override_port) = ep.metadata.opaque_transport_port() {
            // Update the endpoint to target the discovery-provided control
            // plane port.
            ep.addr = (ep.addr.ip(), override_port).into();
        }

        // If there's a destination override, encode that in the opaque
        // transport (i.e. for multicluster gateways). Otherwise, simply
        // encode the original target port. Note that we prefer any port
        // specified in the override to the original destination port.
        let header = ep
            .metadata
            .authority_override()
            .and_then(|auth| {
                let port = auth.port_u16().unwrap_or(orig_port);
                dns::Name::from_str(auth.host())
                    .map_err(|error| warn!(%error, "Invalid name"))
                    .ok()
                    .map(|n| TransportHeader {
                        port,
                        name: Some(n),
                    })
            })
            .unwrap_or(TransportHeader {
                port: orig_port,
                name: None,
            });

        // Connect to the endpoint.
        let connect = self.inner.call(ep);
        Box::pin(async move {
            let mut io = connect.await.map_err(Into::into)?;

            // Once connected, write the opaque header on the socket before
            // returning it.
            if Self::expects_header(&io) {
                let sz = header.write(&mut io).await?;
                debug!(sz, "Wrote header to transport");
            }

            Ok(io)
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::target::{Concrete, Endpoint, Logical};
    use futures::future;
    use linkerd_app_core::{
        io::{self, AsyncWriteExt},
        proxy::api_resolve::{Metadata, ProtocolHint},
        tls,
        transport_header::TransportHeader,
        Conditional,
    };
    use pin_project::pin_project;
    use std::task::Context;
    use tower::util::{service_fn, ServiceExt};

    fn ep(metadata: Metadata) -> Endpoint<()> {
        Endpoint {
            addr: ([127, 0, 0, 2], 4321).into(),
            target_addr: ([127, 0, 0, 2], 4321).into(),
            tls: Conditional::None(tls::NoClientTls::NotProvidedByServiceDiscovery),
            metadata,
            concrete: Concrete {
                resolve: None,
                logical: Logical {
                    orig_dst: ([127, 0, 0, 2], 4321).into(),
                    profile: None,
                    protocol: (),
                },
            },
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn plain() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let svc = OpaqueTransport {
            inner: service_fn(|ep: Endpoint<()>| {
                assert_eq!(ep.addr.port(), 4321);
                future::ready(Ok::<_, io::Error>(Io {
                    io: tokio_test::io::Builder::new().write(b"hello").build(),
                    alpn: None,
                }))
            }),
        };
        let mut io = svc
            .oneshot(ep(Metadata::default()))
            .await
            .expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opaque_no_name() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let svc = OpaqueTransport {
            inner: service_fn(|ep: Endpoint<()>| {
                assert_eq!(ep.addr.port(), 4143);
                let hdr = TransportHeader {
                    port: ep.concrete.logical.orig_dst.port(),
                    name: None,
                };
                let buf = hdr.encode_prefaced_buf().expect("Must encode");
                future::ready(Ok::<_, io::Error>(Io {
                    alpn: Some(tls::NegotiatedProtocol(PROTOCOL)),
                    io: tokio_test::io::Builder::new()
                        .write(&buf[..])
                        .write(b"hello")
                        .build(),
                }))
            }),
        };

        let e = ep(Metadata::new(
            Default::default(),
            ProtocolHint::Unknown,
            Some(4143),
            None,
            None,
        ));
        let mut io = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opaque_named_with_port() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let svc = OpaqueTransport {
            inner: service_fn(|ep: Endpoint<()>| {
                assert_eq!(ep.addr.port(), 4143);
                let hdr = TransportHeader {
                    port: 5555,
                    name: Some(dns::Name::from_str("foo.bar.example.com").unwrap()),
                };
                let buf = hdr.encode_prefaced_buf().expect("Must encode");
                future::ready(Ok::<_, io::Error>(Io {
                    alpn: Some(tls::NegotiatedProtocol(PROTOCOL)),
                    io: tokio_test::io::Builder::new()
                        .write(&buf[..])
                        .write(b"hello")
                        .build(),
                }))
            }),
        };

        let e = ep(Metadata::new(
            Default::default(),
            ProtocolHint::Unknown,
            Some(4143),
            None,
            Some(http::uri::Authority::from_str("foo.bar.example.com:5555").unwrap()),
        ));
        let mut io = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opaque_named_no_port() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let svc = OpaqueTransport {
            inner: service_fn(|ep: Endpoint<()>| {
                assert_eq!(ep.addr.port(), 4143);
                let hdr = TransportHeader {
                    port: ep.concrete.logical.orig_dst.port(),
                    name: None,
                };
                let buf = hdr.encode_prefaced_buf().expect("Must encode");
                future::ready(Ok::<_, io::Error>(Io {
                    alpn: Some(tls::NegotiatedProtocol(PROTOCOL)),
                    io: tokio_test::io::Builder::new()
                        .write(&buf[..])
                        .write(b"hello")
                        .build(),
                }))
            }),
        };

        let e = ep(Metadata::new(
            Default::default(),
            ProtocolHint::Unknown,
            Some(4143),
            None,
            None,
        ));
        let mut io = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[pin_project]
    pub struct Io {
        #[pin]
        io: tokio_test::io::Mock,
        alpn: Option<tls::NegotiatedProtocol<'static>>,
    }

    impl tls::HasNegotiatedProtocol for Io {
        fn negotiated_protocol(&self) -> Option<tls::NegotiatedProtocol<'_>> {
            self.alpn
        }
    }

    impl io::AsyncRead for Io {
        #[inline]
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut io::ReadBuf<'_>,
        ) -> io::Poll<()> {
            self.project().io.poll_read(cx, buf)
        }
    }

    impl io::AsyncWrite for Io {
        #[inline]
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
            self.project().io.poll_shutdown(cx)
        }

        #[inline]
        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
            self.project().io.poll_flush(cx)
        }

        #[inline]
        fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> io::Poll<usize> {
            self.project().io.poll_write(cx, buf)
        }

        #[inline]
        fn poll_write_vectored(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[io::IoSlice<'_>],
        ) -> io::Poll<usize> {
            self.project().io.poll_write_vectored(cx, buf)
        }

        #[inline]
        fn is_write_vectored(&self) -> bool {
            self.io.is_write_vectored()
        }
    }
}
