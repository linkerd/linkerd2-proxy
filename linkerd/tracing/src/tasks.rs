use hyper::Body;
use linkerd_error::Error;
use std::fmt::Write;
use tokio_trace::tasks::TaskList;

#[derive(Clone)]
pub(crate) struct Handle {
    pub(crate) tasks: TaskList,
}

impl Handle {
    pub(crate) fn serve<B>(&self, req: http::Request<B>) -> Result<http::Response<Body>, Error> {
        tracing::debug!("dumping tasks...");
        if accept_json(&req) {
            let body = self.render_json()?;
            Ok(Self::rsp("application/json", body))
        } else {
            Ok(Self::rsp("text/html", self.render_html()))
        }
    }

    fn rsp(typ: &'static str, body: impl Into<Body>) -> http::Response<Body> {
        http::Response::builder()
            .status(http::StatusCode::OK)
            .header(http::header::CONTENT_TYPE, typ)
            .body(body.into())
            .expect("Response must be valid")
    }
}

fn accept_json<B>(req: &http::Request<B>) -> bool {
    let json = http::header::HeaderValue::from_static("application/json");
    req.uri().path().ends_with(".json")
        || req
            .headers()
            .get(http::header::ACCEPT)
            .iter()
            .any(|&value| value == json)
}

impl Handle {
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
                            <th>Spawn Location</th>
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
                    <td>{location}</td>
                </tr>
                ",
                kind = task.kind,
                active = task.is_active(),
                polls = task.polls(),
                total = timings.total_time(),
                busy = timings.busy_time(),
                idle = timings.idle_time(),
                scope = html_escape::encode_text(&task.scope),
                location = html_escape::encode_text(&task.location),
            )
            .expect("writing to a String doesn't fail");
        });
        body.push_str("</tbody></table></body></html>");
        body
    }
}
