//! Facilities for HTTP/1.1 upgrades.
//!
//! HTTP/1.1 specifies an `Upgrade` header field that may be used in tandem with the `Connection`
//! header field as a simple mechanism to transition from HTTP/1.1 to another protocol. This crate
//! provides [`tower`] middleware that enable upgrades to HTTP/2 for services running within a
//! [`tokio`] runtime.
//!
//! Use [`Service::new()`] to add upgrade support to a [`tower::Service`].
//!
//! [RFC 9110 § 7.6.1][rfc9110-connection] for more information about the `Connection` header
//! field, [RFC 9110 § 7.8][rfc9110-upgrade] for more information about HTTP/1.1's `Upgrade`
//! header field, and [RFC 9110 § 15.2.2][rfc9110-101] for more information about the
//! `101 (Switching Protocols)` response status code.
//!
//! Note that HTTP/2 does *NOT* provide support for the `Upgrade` header field, per
//! [RFC 9113 § 8.6][rfc9113]. HTTP/2 is a multiplexed protocol, and connection upgrades are
//! thus inapplicable.
//!
//! [rfc9110-connection]: https://www.rfc-editor.org/rfc/rfc9110#name-connection
//! [rfc9110-upgrade]: https://www.rfc-editor.org/rfc/rfc9110#field.upgrade
//! [rfc9110-101]: https://www.rfc-editor.org/rfc/rfc9110#name-101-switching-protocols
//! [rfc9113]: https://www.rfc-editor.org/rfc/rfc9113.html#name-the-upgrade-header-field

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
