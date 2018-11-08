use http::{self, header::HOST};

/// Settings portion of the `Recognize` key for a request.
///
/// This marks whether to use HTTP/2 or HTTP/1.x for a request. In
/// the case of HTTP/1.x requests, it also stores a "host" key to ensure
/// that each host receives its own connection.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Settings {
    Http1 {
        /// Indicates whether a new service must be created for each request.
        stack_per_request: bool,
        /// Whether the request wants to use HTTP/1.1's Upgrade mechanism.
        ///
        /// Since these cannot be translated into orig-proto, it must be
        /// tracked here so as to allow those with `is_h1_upgrade: true` to
        /// use an explicitly HTTP/1 service, instead of one that might
        /// utilize orig-proto.
        is_h1_upgrade: bool,
        /// Whether or not the request URI was in absolute form.
        ///
        /// This is used to configure Hyper's behaviour at the connection
        /// level, so it's necessary that requests with and without
        /// absolute URIs be bound to separate service stacks. It is also
        /// used to determine what URI normalization will be necessary.
        was_absolute_form: bool,
    },
    Http2,
}

// ===== impl Settings =====

impl Settings {
    pub fn from_request<B>(req: &http::Request<B>) -> Self {
        if req.version() == http::Version::HTTP_2 {
            return Settings::Http2;
        }

        let is_missing_authority = req
            .uri()
            .authority_part()
            .map(|_| false)
            .or_else(|| {
                req.headers()
                    .get(HOST)
                    .and_then(|h| h.to_str().ok())
                    .map(|h| h.is_empty())
            })
            .unwrap_or(true);

        Settings::Http1 {
            was_absolute_form: super::h1::is_absolute_form(req.uri()),
            is_h1_upgrade: super::h1::wants_upgrade(req),
            stack_per_request: is_missing_authority,
        }
    }

    /// Returns true if the request was originally received in absolute form.
    pub fn was_absolute_form(&self) -> bool {
        match self {
            Settings::Http1 {
                was_absolute_form, ..
            } => *was_absolute_form,
            Settings::Http2 => false,
        }
    }

    pub fn can_reuse_clients(&self) -> bool {
        match self {
            Settings::Http1 {
                stack_per_request, ..
            } => !stack_per_request,
            Settings::Http2 => true,
        }
    }

    pub fn is_h1_upgrade(&self) -> bool {
        match self {
            Settings::Http1 { is_h1_upgrade, .. } => *is_h1_upgrade,
            Settings::Http2 => false,
        }
    }

    pub fn is_http2(&self) -> bool {
        match self {
            Settings::Http1 { .. } => false,
            Settings::Http2 => true,
        }
    }
}
