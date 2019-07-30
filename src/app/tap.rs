use super::identity;
use futures::{future, Future};
use logging;
use proxy;
use std::{error, io};
use svc;
use tokio::executor;
use tower_grpc as grpc;
use transport::{
    tls::{self, HasPeerIdentity},
    Listen, Connection,
};
use Conditional;

fn spawn_tap_service<S, B>(
    session: Connection,
    f: impl Future<Item = S, Error = ()> + 'static,
    log: logging::Server,
) -> Result<(), io::Error>
where
    B: tower_grpc::Body + Send + 'static,
    B::Data: Send + 'static,
    S: svc::Service<http::Request<grpc::BoxBody>, Response = http::Response<B>> + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn error::Error + Send + Sync>>,
{
    let log2 = log.clone();
    let f = f.and_then(move |svc| {
        let svc = proxy::grpc::req_box_body::Service::new(svc);
        let svc = proxy::grpc::res_body_as_payload::Service::new(svc);
        let svc = proxy::http::HyperServerSvc::new(svc);

        hyper::server::conn::Http::new()
            .with_executor(log2.executor())
            .http2_only(true)
            .serve_connection(session, svc)
            .map_err(|err| debug!("tap connection error: {}", err))
    });

    executor::current_thread::TaskExecutor::current()
        .spawn_local(Box::new(log.future(f)))
        .map_err(task::Error::into_io)
}

pub fn serve_tap<N, B>(
    bound_port: Listen<identity::Local, ()>,
    tap_svc_name: tls::PeerIdentity,
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

                if let Conditional::Some(ref tap_svc_name) = tap_svc_name {
                    debug!("expecting Tap client name: {:?}", tap_svc_name);

                    let is_expected_name = match session.peer_identity() {
                        Conditional::Some(ref peer_name) => {
                            debug!("found Tap client name: {:?}", peer_name);
                            peer_name == tap_svc_name
                        }
                        Conditional::None(reason) => {
                            debug!("did not find Tap client name: {}", reason);
                            false
                        }
                    };

                    if !is_expected_name {
                        let svc = api::tap::server::TapServer::new(
                            proxy::grpc::unauthenticated::Unauthenticated,
                        );
                        let spawn = spawn_tap_service(session, future::ok(svc), log)
                            .map(|()| new_service);
                        return future::result(spawn);
                    }
                }

                let svc = new_service
                    .make_service(())
                    .map_err(|err| error!("tap MakeService error: {}", err));
                let spawn = spawn_tap_service(session, svc, log)
                    .map(|()| new_service);
                future::result(spawn)
            })
            .map_err(|err| error!("tap listen error: {}", err))
    };

    log.future(fut)
}
