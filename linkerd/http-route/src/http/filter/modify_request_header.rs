use http::header::{HeaderMap, HeaderName, HeaderValue};

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct ModifyRequestHeader {
    pub add: Vec<(HeaderName, HeaderValue)>,
    pub set: Vec<(HeaderName, HeaderValue)>,
    pub remove: Vec<HeaderName>,
}

// === impl ModifyRequestHeader ===

impl ModifyRequestHeader {
    pub fn apply(&self, headers: &mut HeaderMap) {
        for (hdr, val) in &self.add {
            headers.append(hdr, val.clone());
        }
        for (hdr, val) in &self.set {
            headers.insert(hdr, val.clone());
        }
        for hdr in &self.remove {
            headers.remove(hdr);
        }
    }
}
