use linkerd_metrics::prom::{self, encoding::EncodeLabelSet};

pub const BUILD_INFO: BuildInfo = BuildInfo {
    date: env!("LINKERD2_PROXY_BUILD_DATE"),
    git_sha: env!("GIT_SHA"),
    profile: env!("PROFILE"),
    vendor: env!("LINKERD2_PROXY_VENDOR"),
    version: env!("LINKERD2_PROXY_VERSION"),
};

#[derive(Copy, Clone, Debug, Default, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct BuildInfo {
    pub date: &'static str,
    pub git_sha: &'static str,
    pub profile: &'static str,
    pub vendor: &'static str,
    pub version: &'static str,
}

impl BuildInfo {
    pub fn metric(&self) -> prom::Family<BuildInfo, prom::ConstGauge> {
        let fam = prom::Family::<Self, prom::ConstGauge>::new_with_constructor(|| {
            prom::ConstGauge::new(1)
        });
        let _ = fam.get_or_create(self);
        fam
    }
}
