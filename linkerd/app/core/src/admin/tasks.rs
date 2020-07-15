use futures::future;
use http::{header, StatusCode};
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

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        if is_json(&req) {
            let rsp = match self.render_json() {
                Ok(body) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body.into()),
                Err(e) => Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(format!("failed rendering JSON: {}", e).into()),
            }
            .expect("known status code should not fail");

            return futures::future::ok(rsp);
        }

        futures::future::ok(
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/html")
                .body(self.render_html().into())
                .expect("known status code should not fail"),
        )
    }
}

fn is_json<B>(req: &Request<B>) -> bool {
    let json = header::HeaderValue::from_static("application/json");
    req.uri().path().ends_with(".json")
        || req
            .headers()
            .get(header::ACCEPT)
            .iter()
            .any(|&value| value == json)
}

impl Tasks {
    fn render_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(&self.tasks)
    }

    fn render_html(&self) -> String {
        let mut body = String::from(
            "<html>
                <head><title>tasks</title></head>
                <body>
                    <table>
                        <thead>
                        <tr>
                            <th>Kind</th>
                            <th>Active</th>
                            <th>Total Polls</th>
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
                    <td>{polls}</td>
                    <td>{total:?}</td>
                    <td>{busy:?}</td>
                    <td>{idle:?}</td>
                    <td>{scope}</td>
                    <td>{future}</td>
                </tr>
                ",
                kind = task.kind,
                active = task.is_active(),
                polls = task.polls(),
                total = timings.total_time(),
                busy = timings.busy_time(),
                idle = timings.idle_time(),
                scope = html_escape::encode_text(&task.scope),
                future = html_escape::encode_text(&task.future),
            )
            .expect("writing to a String doesn't fail");
        });
        body.push_str("</tbody></table></body></html>");
        body
    }
}

impl fmt::Debug for Tasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tasks").finish()
    }
}
