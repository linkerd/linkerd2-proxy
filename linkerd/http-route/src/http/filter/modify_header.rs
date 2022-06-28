use http::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct ModifyHeader {
    pub add: Vec<(HeaderName, HeaderValue)>,
    pub set: BTreeMap<HeaderName, HeaderValue>,
    pub remove: BTreeSet<HeaderName>,
}

// === impl ModifyRequestHeader ===

impl ModifyHeader {
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
