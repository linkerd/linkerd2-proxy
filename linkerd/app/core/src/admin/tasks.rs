use super::rsp;
use futures::future;
use http::StatusCode;
use hyper::{service::Service, Body, Request, Response};
use std::task::{Context, Poll};
use std::{
    fmt::{self, Write},
    io,
};
use tokio_trace::tasks::TaskList;
#[derive(Clone)]
pub struct Tasks {
    tasks: TaskList,
}

impl From<TaskList> for Tasks {
    fn from(tasks: TaskList) -> Self {
        Self { tasks }
    }
}

impl Service<Request<Body>> for Tasks {
    type Response = Response<Body>;
    type Error = io::Error;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Request<Body>) -> Self::Future {
        let mut body = String::new();
        self.tasks.tasks(|task| {
            writeln!(
                &mut body,
                "kind: {}\nfuture:\n\t{}\ncontext:\n{}\n",
                task.kind, task.future, task.scope
            )
            .expect("writing to a String doesn't fail");
        });
        futures::future::ok(rsp(StatusCode::OK, body))
    }
}

impl fmt::Debug for Tasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tasks").finish()
    }
}
