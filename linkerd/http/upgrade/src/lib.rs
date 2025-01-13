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

/// Removes connection headers from the given [`HeaderMap`][http::HeaderMap].
///
/// An HTTP proxy is required to do this, according to [RFC 9110 § 7.6.1 ¶ 5][rfc9110-761-5]:
///
/// > Intermediaries MUST parse a received Connection header field before a message is forwarded
/// > and, for each connection-option in this field, remove any header or trailer field(s) from the
/// > message with the same name as the connection-option, and then remove the Connection header
/// > field itself (or replace it with the intermediary's own control options for the forwarded
/// > message).
///
/// This function additionally removes some headers mentioned in
/// [RFC 9110 § 7.6.1 ¶ 7-8.5][rfc9110-761-7]
///
/// > Furthermore, intermediaries SHOULD remove or replace fields that are known to require removal
/// > before forwarding, whether or not they appear as a connection-option, after applying those
/// > fields' semantics. This includes but is not limited to:
/// >
/// > - `Proxy-Connection` (Appendix C.2.2 of [HTTP/1.1])
/// > - `Keep-Alive` (Section 19.7.1 of [RFC2068])
/// > - `TE` (Section 10.1.4)
/// > - `Transfer-Encoding` (Section 6.1 of [HTTP/1.1])
/// > - `Upgrade` (Section 7.8)
///
/// [rfc9110-761-5]: https://www.rfc-editor.org/rfc/rfc9110#section-7.6.1-5
/// [rfc9110-761-7]: https://www.rfc-editor.org/rfc/rfc9110#section-7.6.1-7
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
