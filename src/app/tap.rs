use std::error;

use super::identity;
use futures::{future, Future};
use logging;
use proxy;
use svc;
use tokio::executor;
use tower_grpc as grpc;
use transport::{
    tls::{self, HasPeerIdentity},
    Listen,
};
use Conditional;

pub fn serve_tap<N, B>(
    bound_port: Listen<identity::Local, ()>,
    tap_identity: tls::PeerIdentity,
    new_service: N,
) -> impl Future<Item = (), Error = ()> + 'static
where
    B: tower_grpc::Body + Send + 'static,
    B::Data: Send + 'static,
    N: svc::MakeService<(), http::Request<grpc::BoxBody>, Response = http::Response<B>>
        + Send
        + 'static,
    N::Error: Into<Box<dyn error::Error + Send + Sync>>,
    N::MakeError: ::std::fmt::Display,
    <N::Service as svc::Service<http::Request<grpc::BoxBody>>>::Future: Send + 'static,
{
    let log = logging::admin().server("tap", bound_port.local_addr());

    let fut = {
        let log = log.clone();
        bound_port
            .listen_and_fold(new_service, move |mut new_service, (session, remote)| {
                let log = log.clone().with_remote(remote);
                let log_context = log.clone();

                if let Conditional::Some(tap_identity) = tap_identity.as_ref() {
                    match session.peer_identity() {
                        Conditional::Some(ref peer_identity) => {
                            if peer_identity != tap_identity {
                                let svc = api::tap::server::TapServer::new(
                                    proxy::grpc::unauthenticated::Unauthenticated,
                                );
                                let svc = proxy::grpc::req_box_body::Service::new(svc);
                                let svc = proxy::grpc::res_body_as_payload::Service::new(svc);
                                let svc = proxy::http::HyperServerSvc::new(svc);
                                let serve = hyper::server::conn::Http::new()
                                    .with_executor(log_context.executor())
                                    .http2_only(true)
                                    .serve_connection(session, svc)
                                    .map_err(|err| debug!("tap connection error: {}", err));

                                let r = executor::current_thread::TaskExecutor::current()
                                    .spawn_local(Box::new(log.future(serve)))
                                    .map(|()| new_service)
                                    .map_err(task::Error::into_io);

                                return future::result(r);
                            }
                        }
                        Conditional::None(_reason) => {
                            let svc = api::tap::server::TapServer::new(
                                proxy::grpc::unauthenticated::Unauthenticated,
                            );
                            let svc = proxy::grpc::req_box_body::Service::new(svc);
                            let svc = proxy::grpc::res_body_as_payload::Service::new(svc);
                            let svc = proxy::http::HyperServerSvc::new(svc);
                            let serve = hyper::server::conn::Http::new()
                                .with_executor(log_context.executor())
                                .http2_only(true)
                                .serve_connection(session, svc)
                                .map_err(|err| debug!("tap connection error: {}", err));

                            let r = executor::current_thread::TaskExecutor::current()
                                .spawn_local(Box::new(log.future(serve)))
                                .map(|()| new_service)
                                .map_err(task::Error::into_io);

                            return future::result(r);
                        }
                    }
                }

                let serve = new_service
                    .make_service(())
                    .map_err(|err| error!("tap MakeService error: {}", err))
                    .and_then(move |svc| {
                        let svc = proxy::grpc::req_box_body::Service::new(svc);
                        let svc = proxy::grpc::res_body_as_payload::Service::new(svc);
                        let svc = proxy::http::HyperServerSvc::new(svc);
                        hyper::server::conn::Http::new()
                            .with_executor(log_context.executor())
                            .http2_only(true)
                            .serve_connection(session, svc)
                            .map_err(|err| debug!("tap connection error: {}", err))
                    });

                let r = executor::current_thread::TaskExecutor::current()
                    .spawn_local(Box::new(log.future(serve)))
                    .map(|()| new_service)
                    .map_err(task::Error::into_io);
                future::result(r)
            })
            .map_err(|err| error!("tap listen error: {}", err))
    };

    log.future(fut)
}
