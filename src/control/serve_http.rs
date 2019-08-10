use crate::logging;
use crate::transport::{tls, Listen};
use futures::{future, Future};
use hyper::{
    server::conn::Http,
    service::{service_fn, Service},
    Body,
};
use linkerd2_task;
use std::net::SocketAddr;
use tokio::executor::current_thread::TaskExecutor;
use tracing::error;

#[derive(Clone, Debug)]
pub struct ClientAddr(SocketAddr);

pub fn serve_http<L, S>(
    name: &'static str,
    bound_port: Listen<L, ()>,
    service: S,
) -> impl Future<Item = (), Error = ()>
where
    L: tls::listen::HasConfig + Clone + Send + 'static,
    S: Service<ReqBody = Body> + Clone + Send + 'static,
    <S as Service>::Future: Send,
{
    let ename = name.clone();
    let log = logging::admin().server(name, bound_port.local_addr());
    let fut = {
        let log = log.clone();
        bound_port
            .listen_and_fold(Http::new(), move |hyper, (conn, remote)| {
                // Since the `/proxy-log-level` controls access based on the
                // client's IP address, we wrap the service with a new service
                // that adds the remote IP as a request extension.
                let svc = service.clone();
                let svc = service_fn(move |mut req| {
                    let mut svc = svc.clone();
                    req.extensions_mut().insert(ClientAddr(remote));
                    svc.call(req)
                });
                let serve = hyper
                    .serve_connection(conn, svc)
                    .map(|_| {})
                    .map_err(move |e| {
                        error!("error serving {}: {:?}", name, e);
                    });

                let serve = log.clone().with_remote(remote).future(serve);
                let r = TaskExecutor::current()
                    .spawn_local(Box::new(serve))
                    .map(move |()| hyper)
                    .map_err(linkerd2_task::Error::into_io);

                future::result(r)
            })
            .map_err(move |err| error!("{} listener error: {}", ename, err))
    };

    log.future(fut)
}

impl<'a> Into<SocketAddr> for &'a ClientAddr {
    fn into(self) -> SocketAddr {
        self.0
    }
}
