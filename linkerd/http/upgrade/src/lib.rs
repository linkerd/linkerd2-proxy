//! Facilities for HTTP/1 upgrades.

pub use self::upgrade::Service;

pub mod glue;
pub mod upgrade;

pub fn strip_connection_headers(headers: &mut http::HeaderMap) {
    use http::header;
    if let Some(val) = headers.remove(header::CONNECTION) {
        if let Ok(conn_header) = val.to_str() {
            // A `Connection` header may have a comma-separated list of
            // names of other headers that are meant for only this specific
            // connection.
            //
            // Iterate these names and remove them as headers.
            for name in conn_header.split(',') {
                let name = name.trim();
                headers.remove(name);
            }
        }
    }

    // Additionally, strip these "connection-level" headers always, since
    // they are otherwise illegal if upgraded to HTTP2.
    headers.remove(header::UPGRADE);
    headers.remove("proxy-connection");
    headers.remove("keep-alive");
}
