use futures::{future, Future};
use hyper::{server::conn::Http, service::Service, Body};
use tokio::executor::current_thread::TaskExecutor;

use task;
use transport::{tls, Listen};

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
    let log = ::logging::admin().server(name, bound_port.local_addr());
    let fut = {
        let log = log.clone();
        bound_port
            .listen_and_fold(Http::new(), move |hyper, (conn, remote)| {
                let serve = hyper
                    .serve_connection(conn, service.clone())
                    .map(|_| {})
                    .map_err(move |e| {
                        error!("error serving {}: {:?}", name, e);
                    });

                let serve = log.clone().with_remote(remote).future(serve);
                let r = TaskExecutor::current()
                    .spawn_local(Box::new(serve))
                    .map(move |()| hyper)
                    .map_err(task::Error::into_io);

                future::result(r)
            })
            .map_err(move |err| error!("{} listener error: {}", ename, err))
    };

    log.future(fut)
}
