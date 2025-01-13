//! Facilities for HTTP/1.1 upgrades.
//!
//! See [RFC 9110 § 7.8][rfc9110] for more information about HTTP/1.1's `Upgrade` header field,
//! and how it is used to transition to other protocols like HTTP/2 on a particular connection.
//!
//! Note that HTTP/2 does *NOT* provide support for the `Upgrade` header field, per
//! [RFC 9113 § 8.6][rfc9113].
//!
//! > The semantics of `101 (Switching Protocols)` aren't applicable to a multiplexed protocol.
//!
//! Use [`Service::new()`] to add upgrade support to a [`tower::Service`].
//!
//! [rfc9110]: https://www.rfc-editor.org/rfc/rfc9110#field.upgrade
//! [rfc9113]: https://www.rfc-editor.org/rfc/rfc9113.html#name-the-upgrade-header-field

pub use self::svc::Service;

mod body;
pub mod connect;
pub mod svc;
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
