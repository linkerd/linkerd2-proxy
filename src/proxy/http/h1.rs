use bytes::BytesMut;
use http;
use http::header::{CONNECTION, HOST, UPGRADE};
use http::uri::{Authority, Parts, Scheme, Uri};
use std::fmt::Write;
use std::mem;

use super::super::server::Source;
use super::upgrade::HttpConnect;

/// Tries to make sure the `Uri` of the request is in a form needed by
/// hyper's Client.
pub fn normalize_our_view_of_uri<B>(req: &mut http::Request<B>) {
    debug_assert!(
        req.uri().scheme_part().is_none(),
        "normalize_uri shouldn't be called with absolute URIs: {:?}",
        req.uri()
    );

    // try to parse the Host header
    if let Some(auth) = authority_from_host(&req) {
        set_authority(req.uri_mut(), auth);
        return;
    }

    // last resort is to use the so_original_dst
    let orig_dst = req
        .extensions()
        .get::<Source>()
        .and_then(|ctx| ctx.orig_dst_if_not_local());

    if let Some(orig_dst) = orig_dst {
        let mut bytes = BytesMut::with_capacity(31);
        write!(&mut bytes, "{}", orig_dst).expect("socket address display is under 31 bytes");
        let auth =
            Authority::from_shared(bytes.freeze()).expect("socket address is valid authority");
        set_authority(req.uri_mut(), auth);
    }
}

/// Convert any URI into its origin-form (relative path part only).
pub fn set_origin_form(uri: &mut Uri) {
    let mut parts = mem::replace(uri, Uri::default()).into_parts();
    parts.scheme = None;
    parts.authority = None;
    *uri = Uri::from_parts(parts).expect("path only is valid origin-form uri")
}

/// Returns an Authority from a request's Host header.
pub fn authority_from_host<B>(req: &http::Request<B>) -> Option<Authority> {
    super::authority_from_header(req, HOST)
}

fn set_authority(uri: &mut http::Uri, auth: Authority) {
    let mut parts = Parts::from(mem::replace(uri, Uri::default()));

    parts.authority = Some(auth);

    // If this was an origin-form target (path only),
    // then we can't *only* set the authority, as that's
    // an illegal target (such as `example.com/docs`).
    //
    // But don't set a scheme if this was authority-form (CONNECT),
    // since that would change its meaning (like `https://example.com`).
    if parts.path_and_query.is_some() {
        parts.scheme = Some(Scheme::HTTP);
    }

    let new = Uri::from_parts(parts).expect("absolute uri");

    *uri = new;
}

pub fn strip_connection_headers(headers: &mut http::HeaderMap) {
    if let Some(val) = headers.remove(CONNECTION) {
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
    headers.remove(UPGRADE);
    headers.remove("proxy-connection");
    headers.remove("keep-alive");
}

/// Checks requests to determine if they want to perform an HTTP upgrade.
pub fn wants_upgrade<B>(req: &http::Request<B>) -> bool {
    // HTTP upgrades were added in 1.1, not 1.0.
    if req.version() != http::Version::HTTP_11 {
        return false;
    }

    if let Some(upgrade) = req.headers().get(UPGRADE) {
        // If an `h2` upgrade over HTTP/1.1 were to go by the proxy,
        // and it succeeded, there would an h2 connection, but it would
        // be opaque-to-the-proxy, acting as just a TCP proxy.
        //
        // A user wouldn't be able to see any usual HTTP telemetry about
        // requests going over that connection. Instead of that confusion,
        // the proxy strips h2 upgrade headers.
        //
        // Eventually, the proxy will support h2 upgrades directly.
        return upgrade != "h2c";
    }

    // HTTP/1.1 CONNECT requests are just like upgrades!
    req.method() == &http::Method::CONNECT
}

/// Checks responses to determine if they are successful HTTP upgrades.
pub fn is_upgrade<B>(res: &http::Response<B>) -> bool {
    // Upgrades were introduced in HTTP/1.1
    if res.version() != http::Version::HTTP_11 {
        return false;
    }

    // 101 Switching Protocols
    if res.status() == http::StatusCode::SWITCHING_PROTOCOLS {
        return true;
    }

    // CONNECT requests are complete if status code is 2xx.
    if res.extensions().get::<HttpConnect>().is_some() && res.status().is_success() {
        return true;
    }

    // Just a regular HTTP response...
    false
}

/// Returns if the request target is in `absolute-form`.
///
/// This is `absolute-form`: `https://example.com/docs`
///
/// This is not:
///
/// - `/docs`
/// - `example.com`
pub fn is_absolute_form(uri: &Uri) -> bool {
    // It's sufficient just to check for a scheme, since in HTTP1,
    // it's required in absolute-form, and `http::Uri` doesn't
    // allow URIs with the other parts missing when the scheme is set.
    debug_assert!(
        uri.scheme_part().is_none()
            || (uri.authority_part().is_some() && uri.path_and_query().is_some()),
        "is_absolute_form http::Uri invariants: {:?}",
        uri
    );

    uri.scheme_part().is_some()
}

/// Returns if the request target is in `origin-form`.
///
/// This is `origin-form`: `example.com`
fn is_origin_form(uri: &Uri) -> bool {
    uri.scheme_part().is_none() && uri.path_and_query().is_none()
}

/// Returns if the received request is definitely bad.
///
/// Just because a request parses doesn't mean it's correct. For examples:
///
/// - `GET example.com`
/// - `CONNECT /just-a-path
pub fn is_bad_request<B>(req: &http::Request<B>) -> bool {
    if req.method() == &http::Method::CONNECT {
        // CONNECT is only valid over HTTP/1.1
        if req.version() != http::Version::HTTP_11 {
            debug!("CONNECT request not valid for HTTP/1.0: {:?}", req.uri());
            return true;
        }

        // CONNECT requests are only valid in authority-form.
        if !is_origin_form(req.uri()) {
            debug!("CONNECT request with illegal URI: {:?}", req.uri());
            return true;
        }
    // If not CONNECT, refuse any origin-form URIs
    } else if is_origin_form(req.uri()) {
        debug!("{} request with illegal URI: {:?}", req.method(), req.uri());
        return true;
    }

    false
}
