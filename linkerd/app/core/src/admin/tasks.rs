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
        let mut body = String::from(
            "<html>
                <head><title>tasks</title></head>
                <body>
                    <table style=\"width:100%;\">
                        <thead>
                        <tr>
                            <th>Kind</th>
                            <th>Active</th>
                            <th>Total Time</th>
                            <th>Busy Time</th>
                            <th>Idle Time</th>
                            <th>Scope</th>
                            <th>Future</th>
                        </tr>
                        </thead>
                        <tbody>
        ",
        );
        self.tasks.tasks(|task| {
            let timings = task.timings();
            writeln!(
                &mut body,
                "<tr>
                    <td>{kind}</td>
                    <td>{active}</td>
                    <td>{total:?}</td>
                    <td>{busy:?}</td>
                    <td>{idle:?}</td>
                    <th>{scope}</td>
                    <th>{future}</td>
                </tr>
                ",
                kind = task.kind,
                active = task.is_active(),
                total = timings.total_time(),
                busy = timings.busy_time(),
                idle = timings.idle_time(),
                scope = html_escape::encode_text(&task.scope),
                future = html_escape::encode_text(&task.future),
            )
            .expect("writing to a String doesn't fail");
        });
        body.push_str("</tbody></table></body></html>");
        futures::future::ok(
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/html")
                .body(body.into())
                .expect("response"),
        )
    }
}

impl fmt::Debug for Tasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tasks").finish()
    }
}
