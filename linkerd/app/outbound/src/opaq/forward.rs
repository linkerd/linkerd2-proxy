use crate::{tcp, Outbound};
use linkerd_app_core::{io, svc, transport::addrs::*, Error};

#[derive(Debug, thiserror::Error)]
#[error("forward {addr}: {source}")]
pub struct ForwardError {
    addr: Remote<ServerAddr>,
    #[source]
    source: Error,
}

// === impl Outbound ===

impl<C> Outbound<C> {
    pub fn push_opaq_forward<T, I>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<I, Response = (), Error = ForwardError, Future = impl Send> + Clone,
        >,
    >
    where
        T: svc::Param<Remote<ServerAddr>> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        C: svc::MakeConnection<T> + Clone + Send + Sync + 'static,
        C::Connection: Send + Unpin,
        C::Metadata: Send + Unpin,
        C::Future: Send,
    {
        self.map_stack(|_, _, conn| {
            conn.push(svc::stack::WithoutConnectionMetadata::layer())
                .push_new_thunk()
                .push_on_service(tcp::Forward::layer())
                .push(svc::NewMapErr::layer_from_target())
                .push(svc::ArcNewService::layer())
                .check_new_service::<T, I>()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;
    use linkerd_app_core::{
        svc::{self, NewService, ServiceExt},
        transport::{ClientAddr, Local},
    };
    use std::net::SocketAddr;

    #[derive(Clone)]
    struct Target(SocketAddr);

    impl svc::Param<Option<crate::http::Version>> for Target {
        fn param(&self) -> Option<crate::http::Version> {
            None
        }
    }

    impl svc::Param<Remote<ServerAddr>> for Target {
        fn param(&self) -> Remote<ServerAddr> {
            Remote(ServerAddr(self.0))
        }
    }

    #[tokio::test]
    async fn forward() {
        let _trace = linkerd_tracing::test::trace_init();

        let addr = SocketAddr::new([192, 0, 2, 2].into(), 2222);
        let (rt, _shutdown) = runtime();
        let stack = Outbound::new(default_config(), rt)
            .with_stack(svc::mk(move |Target(a): Target| {
                assert_eq!(a, addr);
                let mut io = support::io();
                io.write(b"hello").read(b"world");
                future::ok::<_, support::io::Error>((
                    io.build(),
                    Local(ClientAddr(([0, 0, 0, 0], 0).into())),
                ))
            }))
            .push_opaq_forward()
            .into_inner();

        let mut io = support::io();
        io.read(b"hello").write(b"world");
        stack
            .new_service(Target(addr))
            .oneshot(io.build())
            .await
            .expect("forward must complete successfully");
    }
}

// === impl ForwardError ===

impl<T> From<(&T, Error)> for ForwardError
where
    T: svc::Param<Remote<ServerAddr>>,
{
    fn from((target, source): (&T, Error)) -> Self {
        Self {
            addr: target.param(),
            source,
        }
    }
}
