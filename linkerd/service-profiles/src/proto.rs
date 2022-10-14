use crate::{
    http::{self, RoutePolicy},
    LogicalAddr, Profile, Target,
};
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
    let retry_budget = proto.retry_budget.and_then(convert_retry_budget);
    let http_routes = convert_routes(proto.routes, retry_budget.as_ref());
    let targets = proto
        .dst_overrides
        .into_iter()
        .filter_map(convert_dst_override)
        .collect();
    let endpoint = proto.endpoint.and_then(|e| {
        let labels = std::collections::HashMap::new();
        resolve::to_addr_meta(e, &labels)
    });
    Profile {
        addr: name.map(move |n| LogicalAddr(NameAddr::from((n, port)))),
        http_routes,
        targets,
        opaque_protocol: proto.opaque_protocol,
        endpoint,
    }
}

fn convert_routes(
    orig: impl IntoIterator<Item = api::Route>,
    retry_budget: Option<&Arc<Budget>>,
) -> Option<http::Route> {
    let rules = orig
        .into_iter()
        .filter_map(|rt| convert_route(rt, retry_budget))
        .collect::<Vec<_>>();
    if rules.is_empty() {
        return None;
    }
    Some(http::Route {
        rules,
        hosts: Default::default(),
    })
}

fn convert_route(
    orig: api::Route,
    retry_budget: Option<&Arc<Budget>>,
) -> Option<http::route::Rule<RoutePolicy>> {
    let rsp_classes = orig
        .response_classes
        .into_iter()
        .filter_map(convert_rsp_class)
        .collect();
    let mut policy = http::RoutePolicy::new(orig.metrics_labels.into_iter(), rsp_classes);
    if orig.is_retryable {
        set_route_retry(&mut policy, retry_budget);
    }
    if let Some(timeout) = orig.timeout {
        set_route_timeout(&mut policy, timeout.try_into());
    }

    let mut rules = Vec::new();
    for orig in orig.condition.into_iter().flat_map(|orig| orig.r#match) {
        convert_route_match(orig, &mut rules)
    }
    None
}

fn convert_route_match(rule: api::request_match::Match, dst: &mut Vec<http::route::MatchRequest>) {
    // XXX(eliza): note that this currently basically ignores most forms of
    // nesting. converting nested recursive request match exprs from the API
    // into `MatchRequest`s from `http-route` probably requires some kind of
    // state machine tracking what kind of expression we're inside of...
    match rule {
        api::request_match::Match::All(ms) => {
            // convert `All` req matches into a single http route match expr
            // (probably means building one giant path regex...)
            todo!("eliza: figure this out {ms:?}");
        }
        api::request_match::Match::Any(ms) => {
            // `Any` path matches can be represented as multiple http route
            // rules...unless we're inside of an `All`, in which case, this is
            // going to get hairy...
            todo!("eliza: figure this out {ms:?}");
        }
        api::request_match::Match::Not(m) => {
            // probably requires recursively calling `convert_route_match` with
            // a bit of state telling us we are inside a "not" expr...
            todo!("eliza: figure this out {m:?}");
        }
        api::request_match::Match::Path(api::PathMatch { regex }) => {
            let regex = regex.trim();
            let re = match (regex.starts_with('^'), regex.ends_with('$')) {
                (true, true) => Regex::new(regex).ok(),
                (hd_anchor, tl_anchor) => {
                    let hd = if hd_anchor { "" } else { "^" };
                    let tl = if tl_anchor { "" } else { "$" };
                    let re = format!("{}{}{}", hd, regex, tl);
                    Regex::new(&re).ok()
                }
            };
            match re {
                None => {}
                // TODO(eliza): handle the case where we are inside of an `All`
                // or `Any` expr expr with a method match...
                Some(re) => dst.push(http::route::MatchRequest {
                    path: Some(http::route::MatchPath::Regex(re)),
                    ..Default::default()
                }),
            }
        }
        api::request_match::Match::Method(mm) => {
            let m = mm.r#type.and_then(|m| m.try_into().ok());
            match m {
                // TODO(eliza): handle the case where we are inside of an `All`
                // or `Any` expr expr with a method match...
                Some(m) => dst.push(http::route::MatchRequest {
                    method: Some(m),
                    ..Default::default()
                }),
                None => {}
            }
        }
    }
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

fn set_route_retry(route: &mut http::RoutePolicy, retry_budget: Option<&Arc<Budget>>) {
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
    route: &mut http::RoutePolicy,
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

// fn convert_req_match(orig: api::RequestMatch) ->  {
//     let m = match orig.r#match? {
//         api::request_match::Match::All(ms) => {
//             let ms = ms.matches.into_iter().filter_map(convert_req_match);
//             http::RequestMatch::All(ms.collect())
//         }
//         api::request_match::Match::Any(ms) => {
//             let ms = ms.matches.into_iter().filter_map(convert_req_match);
//             http::RequestMatch::Any(ms.collect())
//         }
//         api::request_match::Match::Not(m) => {
//             let m = convert_req_match(*m)?;
//             http::RequestMatch::Not(Box::new(m))
//         }
//         api::request_match::Match::Path(api::PathMatch { regex }) => {
//             let regex = regex.trim();
//             let re = match (regex.starts_with('^'), regex.ends_with('$')) {
//                 (true, true) => Regex::new(regex).ok()?,
//                 (hd_anchor, tl_anchor) => {
//                     let hd = if hd_anchor { "" } else { "^" };
//                     let tl = if tl_anchor { "" } else { "$" };
//                     let re = format!("{}{}{}", hd, regex, tl);
//                     Regex::new(&re).ok()?
//                 }
//             };
//             http::RequestMatch::Path(Box::new(re))
//         }
//         api::request_match::Match::Method(mm) => {
//             let m = mm.r#type.and_then(|m| m.try_into().ok())?;
//             http::RequestMatch::Method(m)
//         }
//     };

//     Some(m)
// }

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
