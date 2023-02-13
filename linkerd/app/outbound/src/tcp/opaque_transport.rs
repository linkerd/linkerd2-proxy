use crate::{tcp::Connect, ConnectMeta};
use futures::prelude::*;
use linkerd_app_core::{
    dns,
    proxy::http,
    svc, tls,
    transport::{Remote, ServerAddr},
    transport_header::{SessionProtocol, TransportHeader, PROTOCOL},
    Conditional, Error, Result,
};
use std::{
    future::Future,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};
use tracing::{debug, trace, warn};

#[derive(Copy, Clone, Debug)]
pub struct PortOverride(pub u16);

#[derive(Clone, Debug)]
pub struct OpaqueTransport<S> {
    inner: S,
}

// === impl OpaqueTransport ===

impl<S> OpaqueTransport<S> {
    pub fn layer() -> impl svc::Layer<S, Service = Self> + Copy {
        svc::layer::mk(|inner| OpaqueTransport { inner })
    }

    /// Determines whether the connection has negotiated support for the
    /// transport header.
    #[inline]
    fn header_negotiated(meta: &ConnectMeta) -> bool {
        if let Conditional::Some(Some(np)) = meta.tls.as_ref() {
            let tls::NegotiatedProtocolRef(protocol) = np.as_ref();
            return protocol == PROTOCOL;
        }
        false
    }
}

impl<T, S> svc::Service<T> for OpaqueTransport<S>
where
    T: svc::Param<tls::ConditionalClientTls>
        + svc::Param<Remote<ServerAddr>>
        + svc::Param<Option<PortOverride>>
        + svc::Param<Option<http::AuthorityOverride>>
        + svc::Param<Option<SessionProtocol>>,
    S: svc::MakeConnection<Connect, Metadata = ConnectMeta> + Send + 'static,
    S::Connection: Send + Unpin,
    S::Future: Send + 'static,
{
    type Response = (S::Connection, S::Metadata);
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<(S::Connection, S::Metadata)>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, ep: T) -> Self::Future {
        let tls: tls::ConditionalClientTls = ep.param();
        if let tls::ConditionalClientTls::None(reason) = tls {
            trace!(%reason, "Not attempting opaque transport");
            let target = Connect {
                addr: ep.param(),
                tls,
            };
            return Box::pin(self.inner.connect(target).err_into::<Error>());
        }

        // Configure the target port from the endpoint. In opaque cases, this is
        // the application's actual port to be encoded in the header.
        let Remote(ServerAddr(addr)) = ep.param();
        let mut target_port = addr.port();

        // If this endpoint should use opaque transport, then we update the
        // endpoint so the connection actually targets the target proxy's
        // inbound port.
        let connect_port = if let Some(PortOverride(opaque_port)) = ep.param() {
            debug!(target_port, opaque_port, "Using opaque transport");
            opaque_port
        } else {
            trace!("No port override");
            target_port
        };

        // If an authority override is present, we're communicating with a
        // remote gateway:
        // - The target port will already be the proxy's inbound port, so
        //   override it from the authority.
        // - Encode the name from the authority override so the gateway can
        //   route the connection appropriately.
        let mut name = None;
        if let Some(http::AuthorityOverride(authority)) = ep.param() {
            if let Some(override_port) = authority.port_u16() {
                name = dns::Name::from_str(authority.host())
                    .map_err(|error| warn!(%error, "Invalid name"))
                    .ok();
                target_port = override_port;
                debug!(?name, target_port, "Using authority override");
            }
        }

        let protocol: Option<SessionProtocol> = ep.param();

        debug!(?protocol, "Using session protocol");

        let connect = self.inner.connect(Connect {
            addr: Remote(ServerAddr((addr.ip(), connect_port).into())),
            tls,
        });
        Box::pin(async move {
            let (mut io, meta) = connect.await.map_err(Into::into)?;

            // If transport header support has been negotiated via ALPN, encode
            // the header and then return the socket.
            if Self::header_negotiated(&meta) {
                let header = TransportHeader {
                    port: target_port,
                    name,
                    protocol,
                };
                trace!(?header, "Writing transport header");
                let sz = header.write(&mut io).await?;
                debug!(sz, "Wrote transport header");
            } else {
                trace!("Connection does not expect a transport header");
            }

            Ok((io, meta))
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use futures::future;
    use linkerd_app_core::{
        identity,
        io::{self, AsyncWriteExt},
        proxy::{
            api_resolve::{Metadata, ProtocolHint},
            http,
        },
        tls,
        transport::{ClientAddr, Local},
        transport_header::TransportHeader,
    };
    use tower::util::{service_fn, ServiceExt};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Endpoint {
        metadata: Metadata,
        http: Option<http::Version>,
    }

    fn ep(metadata: Metadata, http: impl Into<Option<http::Version>>) -> Endpoint {
        Endpoint {
            metadata,
            http: http.into(),
        }
    }

    impl svc::Param<Remote<ServerAddr>> for Endpoint {
        fn param(&self) -> Remote<ServerAddr> {
            Remote(ServerAddr(([127, 0, 0, 2], 4321).into()))
        }
    }

    impl svc::Param<tls::ConditionalClientTls> for Endpoint {
        // This is the same logic as the `client_tls` function in `endpoint.rs`
        fn param(&self) -> tls::ConditionalClientTls {
            // If we're transporting an opaque protocol OR we're communicating with
            // a gateway, then set an ALPN value indicating support for a transport
            // header.
            let use_transport_header = self.metadata.opaque_transport_port().is_some()
                || self.metadata.authority_override().is_some();

            self.metadata
                .identity()
                .cloned()
                .map(move |server_id| {
                    Conditional::Some(tls::ClientTls {
                        server_id,
                        alpn: if use_transport_header {
                            Some(tls::client::AlpnProtocols(vec![PROTOCOL.into()]))
                        } else {
                            None
                        },
                    })
                })
                .unwrap_or(Conditional::None(
                    tls::NoClientTls::NotProvidedByServiceDiscovery,
                ))
        }
    }

    impl svc::Param<Option<PortOverride>> for Endpoint {
        fn param(&self) -> Option<PortOverride> {
            self.metadata.opaque_transport_port().map(PortOverride)
        }
    }

    impl svc::Param<Option<http::AuthorityOverride>> for Endpoint {
        fn param(&self) -> Option<http::AuthorityOverride> {
            self.metadata
                .authority_override()
                .cloned()
                .map(http::AuthorityOverride)
        }
    }

    impl svc::Param<Option<SessionProtocol>> for Endpoint {
        // This is the same logic as the `Param<Option<SessionProtocol>>` impl
        // for `Connect<T>`.
        fn param(&self) -> Option<SessionProtocol> {
            // The discovered protocol hint indicates that this endpoint will treat
            // all connections as opaque TCP streams. Don't send our detected
            // session protocol as part of a transport header.
            if self.metadata.protocol_hint() == ProtocolHint::Opaque {
                return None;
            }

            match self.http? {
                http::Version::Http1 => Some(SessionProtocol::Http1),
                http::Version::H2 => Some(SessionProtocol::Http2),
            }
        }
    }

    fn expect_header(
        header: TransportHeader,
    ) -> impl Fn(Connect) -> futures::future::Ready<Result<(tokio_test::io::Mock, ConnectMeta), io::Error>>
    {
        move |ep| {
            let Remote(ServerAddr(sa)) = ep.addr;
            assert_eq!(sa.port(), 4143);
            assert!(ep.tls.is_some());
            let buf = header.encode_prefaced_buf().expect("Must encode");
            let io = tokio_test::io::Builder::new()
                .write(&buf[..])
                .write(b"hello")
                .build();
            let meta = tls::ConnectMeta {
                socket: Local(ClientAddr(([0, 0, 0, 0], 0).into())),
                tls: Conditional::Some(Some(tls::NegotiatedProtocolRef(PROTOCOL).into())),
            };
            future::ready(Ok::<_, io::Error>((io, meta)))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn plain() {
        let _trace = linkerd_tracing::test::trace_init();

        let svc = OpaqueTransport {
            inner: service_fn(|ep: Connect| {
                let Remote(ServerAddr(sa)) = ep.addr;
                assert_eq!(sa.port(), 4321);
                assert!(ep.tls.is_none());
                let io = tokio_test::io::Builder::new().write(b"hello").build();
                let meta = tls::ConnectMeta {
                    socket: Local(ClientAddr(([0, 0, 0, 0], 0).into())),
                    tls: Conditional::Some(None),
                };
                future::ready(Ok::<_, io::Error>((io, meta)))
            }),
        };
        let (mut io, _meta) = svc
            .oneshot(ep(Metadata::default(), None))
            .await
            .expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_no_name() {
        let _trace = linkerd_tracing::test::trace_init();

        let svc = OpaqueTransport {
            inner: service_fn(expect_header(TransportHeader {
                port: 4321,
                name: None,
                protocol: None,
            })),
        };

        let e = ep(
            Metadata::new(
                None,
                ProtocolHint::Unknown,
                Some(4143),
                Some(tls::ServerId(
                    identity::Name::from_str("server.id").unwrap(),
                )),
                None,
            ),
            None,
        );
        let (mut io, _meta) = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_named_with_port() {
        let _trace = linkerd_tracing::test::trace_init();

        let svc = OpaqueTransport {
            inner: service_fn(expect_header(TransportHeader {
                port: 5555,
                name: Some(dns::Name::from_str("foo.bar.example.com").unwrap()),
                protocol: None,
            })),
        };

        let e = ep(
            Metadata::new(
                None,
                ProtocolHint::Unknown,
                Some(4143),
                Some(tls::ServerId(
                    identity::Name::from_str("server.id").unwrap(),
                )),
                Some(http::uri::Authority::from_str("foo.bar.example.com:5555").unwrap()),
            ),
            None,
        );
        let (mut io, _meta) = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_named_no_port() {
        let _trace = linkerd_tracing::test::trace_init();

        let svc = OpaqueTransport {
            inner: service_fn(expect_header(TransportHeader {
                port: 4321,
                name: None,
                protocol: None,
            })),
        };

        let e = ep(
            Metadata::new(
                None,
                ProtocolHint::Unknown,
                Some(4143),
                Some(tls::ServerId(
                    identity::Name::from_str("server.id").unwrap(),
                )),
                None,
            ),
            None,
        );
        let (mut io, _meta) = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opaque_no_name() {
        let _trace = linkerd_tracing::test::trace_init();

        let svc = OpaqueTransport {
            inner: service_fn(expect_header(TransportHeader {
                port: 4321,
                name: None,
                protocol: None,
            })),
        };

        let e = ep(
            Metadata::new(
                None,
                ProtocolHint::Opaque,
                Some(4143),
                Some(tls::ServerId(
                    identity::Name::from_str("server.id").unwrap(),
                )),
                None,
            ),
            None,
        );
        let (mut io, _meta) = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opaque_named_with_port() {
        let _trace = linkerd_tracing::test::trace_init();

        let svc = OpaqueTransport {
            inner: service_fn(expect_header(TransportHeader {
                port: 5555,
                name: Some(dns::Name::from_str("foo.bar.example.com").unwrap()),
                protocol: None,
            })),
        };

        let e = ep(
            Metadata::new(
                None,
                ProtocolHint::Opaque,
                Some(4143),
                Some(tls::ServerId(
                    identity::Name::from_str("server.id").unwrap(),
                )),
                Some(http::uri::Authority::from_str("foo.bar.example.com:5555").unwrap()),
            ),
            None,
        );
        let (mut io, _meta) = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opaque_named_no_port() {
        let _trace = linkerd_tracing::test::trace_init();

        let svc = OpaqueTransport {
            inner: service_fn(expect_header(TransportHeader {
                port: 4321,
                name: None,
                protocol: None,
            })),
        };

        let e = ep(
            Metadata::new(
                None,
                ProtocolHint::Opaque,
                Some(4143),
                Some(tls::ServerId(
                    identity::Name::from_str("server.id").unwrap(),
                )),
                None,
            ),
            None,
        );
        let (mut io, _meta) = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_http1() {
        let _trace = linkerd_tracing::test::trace_init();

        let svc = OpaqueTransport {
            inner: service_fn(expect_header(TransportHeader {
                port: 5555,
                name: Some(dns::Name::from_str("foo.bar.example.com").unwrap()),
                protocol: Some(SessionProtocol::Http1),
            })),
        };

        let e = ep(
            Metadata::new(
                None,
                ProtocolHint::Unknown,
                Some(4143),
                Some(tls::ServerId(
                    identity::Name::from_str("server.id").unwrap(),
                )),
                Some(http::uri::Authority::from_str("foo.bar.example.com:5555").unwrap()),
            ),
            http::Version::Http1,
        );
        let (mut io, _meta) = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opaque_http1() {
        let _trace = linkerd_tracing::test::trace_init();

        let svc = OpaqueTransport {
            inner: service_fn(expect_header(TransportHeader {
                port: 5555,
                name: Some(dns::Name::from_str("foo.bar.example.com").unwrap()),
                // If the endpoint's protocol hint is opaque, no session
                // should be sent.
                protocol: None,
            })),
        };

        let e = ep(
            Metadata::new(
                None,
                ProtocolHint::Opaque,
                Some(4143),
                Some(tls::ServerId(
                    identity::Name::from_str("server.id").unwrap(),
                )),
                Some(http::uri::Authority::from_str("foo.bar.example.com:5555").unwrap()),
            ),
            http::Version::Http1,
        );
        let (mut io, _meta) = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hinted_http2() {
        let _trace = linkerd_tracing::test::trace_init();

        let svc = OpaqueTransport {
            inner: service_fn(expect_header(TransportHeader {
                port: 5555,
                name: Some(dns::Name::from_str("foo.bar.example.com").unwrap()),
                // If the endpoint contains an upgrade hint, we should send the
                // upgraded session protocol, rather than the detected protocol.
                protocol: Some(SessionProtocol::Http2),
            })),
        };

        let e = ep(
            Metadata::new(
                None,
                ProtocolHint::Http2,
                Some(4143),
                Some(tls::ServerId(
                    identity::Name::from_str("server.id").unwrap(),
                )),
                Some(http::uri::Authority::from_str("foo.bar.example.com:5555").unwrap()),
            ),
            http::Version::H2,
        );
        let (mut io, _meta) = svc.oneshot(e).await.expect("Connect must not fail");
        io.write_all(b"hello").await.expect("Write must succeed");
    }
}
