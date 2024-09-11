use std::cmp::Ordering;

use crate::SessionInfo;

/// Matches TLS sessions. For now, this is a placeholder
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct MatchSession(());

/// Summarizes a matched TLS session. For now this is a placeholder
#[derive(Clone, Debug, Hash, PartialEq, Eq, Default)]
pub struct SessionMatch(());

impl MatchSession {
    pub(crate) fn match_session(&self, _: &SessionInfo) -> Option<SessionMatch> {
        Some(SessionMatch::default())
    }
}

impl std::cmp::PartialOrd for SessionMatch {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for SessionMatch {
    fn cmp(&self, _: &Self) -> std::cmp::Ordering {
        Ordering::Equal
    }
}
