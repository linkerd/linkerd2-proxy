use crate::{MatchSni, SessionInfo, SniMatch};

/// Matches TLS sessions.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct MatchSession {
    pub sni: Option<MatchSni>,
}

/// Summarizes a matched TLS session.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct SessionMatch {
    sni: SniMatch,
}

impl MatchSession {
    pub(crate) fn match_session(&self, info: &SessionInfo) -> Option<SessionMatch> {
        let mut summary = SessionMatch::default();

        if let Some(match_sni) = &self.sni {
            if let Some(sni) = match_sni.summarize_match(&info.sni) {
                summary.sni = sni;
            } else {
                return None;
            }
        }
        Some(summary)
    }
}

// === impl SessionMatch ===

impl Default for SessionMatch {
    fn default() -> Self {
        Self {
            sni: SniMatch::Exact(0),
        }
    }
}

impl std::cmp::PartialOrd for SessionMatch {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for SessionMatch {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sni.cmp(&other.sni)
    }
}
