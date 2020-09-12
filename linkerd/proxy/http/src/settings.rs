/// HTTP Client Settings portion of the `Recognize` key for a request.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Settings {
    Http1,
    Http2,
}

// ===== impl Settings =====

impl Settings {
    pub fn from_request<B>(req: &http::Request<B>) -> Self {
        if req.version() == http::Version::HTTP_2 {
            return Settings::Http2;
        }

        Settings::Http1
    }
}
