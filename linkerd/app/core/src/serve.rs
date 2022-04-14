use crate::{
    io,
    svc::{self, Param},
    transport::{ClientAddr, Remote},
    Result,
};
use futures::prelude::*;
use linkerd_error::Error;
use tower::util::ServiceExt;
use tracing::{debug, debug_span, info, instrument::Instrument, warn};

/// Spawns a task that binds an `L`-typed listener with an `A`-typed connection-accepting service.
///
/// The task is driven until shutdown is signaled.
pub async fn serve<M, S, I, A>(
    listen: impl Stream<Item = Result<(A, I)>>,
    new_accept: M,
    shutdown: impl Future,
) where
    I: Send + 'static,
    A: Param<Remote<ClientAddr>>,
    M: svc::NewService<A, Service = S>,
    S: tower::Service<io::ScopedIo<I>, Response = ()> + Send + 'static,
    S::Error: Into<Error>,
    S::Future: Send + 'static,
{
    let accept = async move {
        futures::pin_mut!(listen);
        loop {
            match listen.next().await {
                None => return,
                Some(conn) => {
                    // If the listener returned an error, complete the task.
                    let (addrs, io) = match conn {
                        Ok(conn) => conn,
                        Err(error) => {
                            warn!(error, "Server failed to accept connection");
                            continue;
                        }
                    };

                    // The local addr should be instrumented from the listener's context.
                    let Remote(ClientAddr(client_addr)) = addrs.param();
                    let span = debug_span!("accept", client.addr = %client_addr).entered();
                    let accept = new_accept.new_service(addrs);

                    // Dispatch all of the work for a given connection onto a
                    // connection-specific task.
                    tokio::spawn(
                        async move {
                            match accept.ready_oneshot().err_into::<Error>().await {
                                Ok(mut accept) => {
                                    match accept
                                        .call(io::ScopedIo::server(io))
                                        .err_into::<Error>()
                                        .await
                                    {
                                        Ok(()) => debug!("Connection closed"),
                                        Err(reason) if is_io(&*reason) => {
                                            debug!(%reason, "Connection closed")
                                        }
                                        Err(error) => {
                                            info!(error, client.addr = %client_addr, "Connection closed")
                                        }
                                    }
                                    // Hold the service until the connection is complete. This
                                    // helps tie any inner cache lifetimes to the services they
                                    // return.
                                    drop(accept);
                                }
                                Err(error) => {
                                    warn!(error, client.addr = %client_addr, "Server failed to become ready");
                                }
                            }
                        }
                        .instrument(span.exit().or_current()),
                    );
                }
            }
        }
    }
    .in_current_span();

    // Stop the accept loop when the shutdown signal fires.
    //
    // This ensures that the accept service's readiness can't block shutdown.
    tokio::select! {
        res = accept => { res }
        _ = shutdown => {}
    }
}

fn is_io(e: &(dyn std::error::Error + 'static)) -> bool {
    e.is::<io::Error>() || e.source().map(is_io).unwrap_or(false)
}
