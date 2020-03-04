use http::{self, header::HOST};

/// HTTP Client Settings portion of the `Recognize` key for a request.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Settings {
    Http1 {
        /// Indicates whether connections can be reused for each request.
        keep_alive: bool,
        /// Whether a request has a `CONNECT` method or `Upgrade` header.
        wants_h1_upgrade: bool,
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

pub trait HasSettings {
    fn http_settings(&self) -> &Settings;
}

impl HasSettings for Settings {
    fn http_settings(&self) -> &Settings {
        self
    }
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

        let wants_h1_upgrade = super::h1::wants_upgrade(&req);

        Settings::Http1 {
            keep_alive: !is_missing_authority,
            wants_h1_upgrade,
            was_absolute_form: super::h1::is_absolute_form(req.uri()),
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

    pub fn is_http2(&self) -> bool {
        match self {
            Settings::Http2 => true,
            Settings::Http1 { .. } => false,
        }
    }
}
