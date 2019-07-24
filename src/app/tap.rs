use super::identity;
use futures::{future, Future};
use logging;
use proxy;
use std::error;
use svc;
use tokio::executor;
use tower_grpc as grpc;
use transport::{
    tls::{self, HasPeerIdentity},
    Listen,
};
use Conditional;

macro_rules! tap_server {
    { $session:ident, $svc:ident, $log_context:ident } => {{
        let svc = proxy::grpc::req_box_body::Service::new($svc);
        let svc = proxy::grpc::res_body_as_payload::Service::new(svc);
        let svc = proxy::http::HyperServerSvc::new(svc);

        hyper::server::conn::Http::new()
            .with_executor($log_context.executor())
            .http2_only(true)
            .serve_connection($session, svc)
            .map_err(|err| debug!("tap connection error: {}", err))
    };}
}

macro_rules! tap_task {
    { $srv:ident, $log:ident, $new_service:ident } => {{
        executor::current_thread::TaskExecutor::current()
            .spawn_local(Box::new($log.future($srv)))
            .map(|()| $new_service)
            .map_err(task::Error::into_io)
    };}
}

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

                if let Conditional::Some(ref tap_identity) = tap_identity {
                    debug!("expected Tap client identity: {:?}", tap_identity);

                    let is_expected_identity = match session.peer_identity() {
                        Conditional::Some(ref peer_identity) => {
                            debug!("found Tap client identity: {:?}", peer_identity);
                            peer_identity == tap_identity
                        }
                        _ => {
                            debug!("did not find Tap client identity");
                            false
                        }
                    };

                    if !is_expected_identity {
                        let svc = api::tap::server::TapServer::new(
                            proxy::grpc::unauthenticated::Unauthenticated,
                        );
                        let srv = tap_server!(session, svc, log_context);
                        let task = tap_task!(srv, log, new_service);

                        return future::result(task);
                    }
                }

                let srv = new_service
                    .make_service(())
                    .map_err(|err| error!("tap MakeService error: {}", err))
                    .and_then(move |svc| tap_server!(session, svc, log_context));
                let task = tap_task!(srv, log, new_service);

                future::result(task)
            })
            .map_err(|err| error!("tap listen error: {}", err))
    };

    log.future(fut)
}
