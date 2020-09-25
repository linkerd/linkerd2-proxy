use crate::svc;
use futures::prelude::*;
use linkerd2_error::Error;
use linkerd2_proxy_transport::listen::Addrs;
use tower::util::ServiceExt;
use tracing::{debug, info, info_span};
use tracing_futures::Instrument;

/// Spawns a task that binds an `L`-typed listener with an `A`-typed
/// connection-accepting service.
///
/// The task is driven until shutdown is signaled.
pub async fn serve<M, A, I>(
    listen: impl Stream<Item = std::io::Result<(Addrs, I)>>,
    mut new_accept: M,
    shutdown: impl Future,
) -> Result<(), Error>
where
    I: Send + 'static,
    M: svc::NewService<Addrs, Service = A>,
    A: tower::Service<I, Response = ()> + Send + 'static,
    A::Error: Into<Error>,
    A::Future: Send + 'static,
{
    let accept = async move {
        futures::pin_mut!(listen);
        loop {
            match listen.next().await {
                None => return Ok(()),
                Some(conn) => {
                    // If the listener returned an error, complete the task
                    let (addrs, io) = conn?;

                    // The local addr should be instrumented from the listener's context.
                    let span = info_span!(
                        "accept",
                        peer.addr = %addrs.peer(),
                        target.addr = %addrs.target_addr(),
                    );

                    let accept = new_accept.new_service(addrs);

                    // Dispatch all of the work for a given connection onto a connection-specific task.
                    tokio::spawn(
                        async move {
                            match accept.oneshot(io).err_into::<Error>().await {
                                Ok(()) => debug!("Connection closed"),
                                Err(error) => info!(%error, "Connection closed"),
                            }
                        }
                        .instrument(span),
                    );
                }
            }
        }
    };

    // Stop the accept loop when the shutdown signal fires.
    //
    // This ensures that the accept service's readiness can't block shutdown.
    tokio::select! {
        res = accept => { res }
        _ = shutdown => { Ok(()) }
    }
}
