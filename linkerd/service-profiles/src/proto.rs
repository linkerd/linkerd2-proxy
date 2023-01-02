use crate::{http, tcp, LogicalAddr, Profile, Target, Targets};
use ahash::AHashSet;
use linkerd2_proxy_api::destination as api;
use linkerd_addr::NameAddr;
use linkerd_dns_name::Name;
use linkerd_proxy_api_resolve::pb as resolve;
use regex::Regex;
use std::{str::FromStr, sync::Arc, time::Duration};
use tower::retry::budget::Budget;
use tracing::warn;

pub(super) fn convert_profile(proto: api::DestinationProfile, port: u16) -> Profile {
    let name = Name::from_str(&proto.fully_qualified_name).ok();
    let addr = name.map(|n| NameAddr::from((n, port)));

    let mut targets = proto
        .dst_overrides
        .into_iter()
        .filter_map(convert_dst_override)
        .collect::<Targets>();
    if targets.is_empty() {
        if let Some(addr) = addr.clone() {
            targets = std::iter::once(Target { addr, weight: 1 }).collect();
        }
    }

    let http_routes = {
        // Default route used when no routes in the profile match a request.
        let default = std::iter::once((
            http::RequestMatch::default(),
            http::Route::new(std::iter::empty(), Vec::new(), targets.clone()),
        ));
        let retry_budget = proto.retry_budget.and_then(convert_retry_budget);
        let tgts = &targets;
        proto
            .routes
            .into_iter()
            .filter_map(move |orig| convert_route(orig, retry_budget.as_ref(), tgts))
            // Populate the default route last, so that every other route is tried first.
            .chain(default)
            .collect()
    };

    let endpoint = proto.endpoint.and_then(|e| {
        let labels = std::collections::HashMap::new();
        resolve::to_addr_meta(e, &labels)
    });

    let target_addrs = targets
        .iter()
        .map(|t| t.addr.clone())
        .collect::<AHashSet<_, _>>();

    // ServiceProfiles don't define TCP routes, generate a single match which
    // matches all TCP connections but still uses the list of targets from the profile.
    let tcp_routes =
        std::iter::once((tcp::RequestMatch::default(), tcp::Route::new(targets))).collect();

    Profile {
        addr: addr.map(LogicalAddr),
        http_routes,
        tcp_routes,
        opaque_protocol: proto.opaque_protocol,
        target_addrs: target_addrs.into(),
        endpoint,
    }
}

fn convert_route(
    orig: api::Route,
    retry_budget: Option<&Arc<Budget>>,
    targets: &Targets,
) -> Option<(http::RequestMatch, http::Route)> {
    let req_match = orig.condition.and_then(convert_req_match)?;
    let rsp_classes = orig
        .response_classes
        .into_iter()
        .filter_map(convert_rsp_class)
        .collect();
    let mut route = http::Route::new(
        orig.metrics_labels.into_iter(),
        rsp_classes,
        targets.clone(),
    );
    if orig.is_retryable {
        set_route_retry(&mut route, retry_budget);
    }
    if let Some(timeout) = orig.timeout {
        set_route_timeout(&mut route, timeout.try_into());
    }
    Some((req_match, route))
}

fn convert_dst_override(orig: api::WeightedDst) -> Option<Target> {
    if orig.weight == 0 {
        return None;
    }
    let addr = NameAddr::from_str(orig.authority.as_str()).ok()?;
    Some(Target {
        addr,
        weight: orig.weight,
    })
}

fn set_route_retry(route: &mut http::Route, retry_budget: Option<&Arc<Budget>>) {
    let budget = match retry_budget {
        Some(budget) => budget.clone(),
        None => {
            warn!("retry_budget is missing: {:?}", route);
            return;
        }
    };

    route.set_retries(budget);
}

fn set_route_timeout(
    route: &mut http::Route,
    timeout: Result<Duration, prost_types::DurationError>,
) {
    match timeout {
        Ok(dur) => {
            route.set_timeout(dur);
        }
        Err(error) => {
            warn!(%error, "error setting timeout for route");
        }
    }
}

fn convert_req_match(orig: api::RequestMatch) -> Option<http::RequestMatch> {
    let m = match orig.r#match? {
        api::request_match::Match::All(ms) => {
            let ms = ms.matches.into_iter().filter_map(convert_req_match);
            http::RequestMatch::All(ms.collect())
        }
        api::request_match::Match::Any(ms) => {
            let ms = ms.matches.into_iter().filter_map(convert_req_match);
            http::RequestMatch::Any(ms.collect())
        }
        api::request_match::Match::Not(m) => {
            let m = convert_req_match(*m)?;
            http::RequestMatch::Not(Box::new(m))
        }
        api::request_match::Match::Path(api::PathMatch { regex }) => {
            let regex = regex.trim();
            let re = match (regex.starts_with('^'), regex.ends_with('$')) {
                (true, true) => Regex::new(regex).ok()?,
                (hd_anchor, tl_anchor) => {
                    let hd = if hd_anchor { "" } else { "^" };
                    let tl = if tl_anchor { "" } else { "$" };
                    let re = format!("{}{}{}", hd, regex, tl);
                    Regex::new(&re).ok()?
                }
            };
            http::RequestMatch::Path(re.into())
        }
        api::request_match::Match::Method(mm) => {
            let m = mm.r#type.and_then(|m| m.try_into().ok())?;
            http::RequestMatch::Method(m)
        }
    };

    Some(m)
}

fn convert_rsp_class(orig: api::ResponseClass) -> Option<http::ResponseClass> {
    let c = orig.condition.and_then(convert_rsp_match)?;
    Some(http::ResponseClass::new(orig.is_failure, c))
}

fn convert_rsp_match(orig: api::ResponseMatch) -> Option<http::ResponseMatch> {
    let m = match orig.r#match? {
        api::response_match::Match::All(ms) => {
            let ms = ms
                .matches
                .into_iter()
                .filter_map(convert_rsp_match)
                .collect::<Vec<_>>();
            if ms.is_empty() {
                return None;
            }
            http::ResponseMatch::All(ms)
        }
        api::response_match::Match::Any(ms) => {
            let ms = ms
                .matches
                .into_iter()
                .filter_map(convert_rsp_match)
                .collect::<Vec<_>>();
            if ms.is_empty() {
                return None;
            }
            http::ResponseMatch::Any(ms)
        }
        api::response_match::Match::Not(m) => {
            let m = convert_rsp_match(*m)?;
            http::ResponseMatch::Not(Box::new(m))
        }
        api::response_match::Match::Status(range) => {
            let min = ::http::StatusCode::from_u16(range.min as u16).ok()?;
            let max = ::http::StatusCode::from_u16(range.max as u16).ok()?;
            http::ResponseMatch::Status { min, max }
        }
    };

    Some(m)
}

fn convert_retry_budget(orig: api::RetryBudget) -> Option<Arc<Budget>> {
    let min_retries = if orig.min_retries_per_second <= i32::MAX as u32 {
        orig.min_retries_per_second
    } else {
        warn!(
            "retry_budget min_retries_per_second overflow: {:?}",
            orig.min_retries_per_second
        );
        return None;
    };

    let retry_ratio = orig.retry_ratio;
    if !(0.0..=1000.0).contains(&retry_ratio) {
        warn!("retry_budget retry_ratio invalid: {:?}", retry_ratio);
        return None;
    }

    let ttl = match orig.ttl {
        Some(pb_dur) => {
            if pb_dur.nanos > 1_000_000_000 {
                warn!("retry_budget nanos must not exceed 1s");
                return None;
            }
            if pb_dur.seconds < 0 || pb_dur.nanos < 0 {
                warn!("retry_budget ttl negative");
                return None;
            }
            match pb_dur.try_into() {
                Ok(dur) => {
                    if dur > Duration::from_secs(60) || dur < Duration::from_secs(1) {
                        warn!("retry_budget ttl invalid: {:?}", dur);
                        return None;
                    }
                    dur
                }
                Err(negative) => {
                    warn!("retry_budget ttl negative: {:?}", negative);
                    return None;
                }
            }
        }
        None => {
            warn!("retry_budget ttl missing");
            return None;
        }
    };

    Some(Arc::new(Budget::new(ttl, min_retries, retry_ratio)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck::*;

    quickcheck! {
        fn retry_budget_from_proto(
            min_retries_per_second: u32,
            retry_ratio: f32,
            seconds: i64,
            nanos: i32
        ) -> bool {
            let proto = api::RetryBudget {
                min_retries_per_second,
                retry_ratio,
                ttl: Some(prost_types::Duration {
                    seconds,
                    nanos,
                }),
            };
            // Ensure we don't panic.
            convert_retry_budget(proto);
            true
        }
    }
}
