use super::ParseError;
use linkerd_app_core::{dns, identity, Addr, IpNet};
use rangemap::RangeInclusiveSet;
use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    time::Duration,
};
use tracing::error;

pub(crate) fn parse_dns_name(s: &str) -> Result<dns::Name, ParseError> {
    s.parse().map_err(|_| {
        error!("Not a valid identity name: {s}");
        ParseError::NameError
    })
}

pub(crate) fn parse_identity(s: &str) -> Result<identity::Id, ParseError> {
    s.parse().map_err(|_| {
        error!("Not a valid identity name: {s}");
        ParseError::NameError
    })
}

pub(super) fn parse_bool(s: &str) -> Result<bool, ParseError> {
    s.parse().map_err(Into::into)
}

pub(super) fn parse_number<T>(s: &str) -> Result<T, ParseError>
where
    T: FromStr,
    ParseError: From<T::Err>,
{
    s.parse().map_err(Into::into)
}

pub(super) fn parse_duration_opt(s: &str) -> Result<Option<Duration>, ParseError> {
    if s.is_empty() {
        return Ok(None);
    }
    parse_duration(s).map(Some)
}

pub(super) fn parse_duration(s: &str) -> Result<Duration, ParseError> {
    use regex::Regex;

    let re = Regex::new(r"^\s*(\d+)(ms|s|m|h|d)?\s*$").expect("duration regex");

    let cap = re.captures(s).ok_or(ParseError::NotADuration)?;

    let magnitude = parse_number(&cap[1])?;
    match cap.get(2).map(|m| m.as_str()) {
        None if magnitude == 0 => Ok(Duration::from_secs(0)),
        Some("ms") => Ok(Duration::from_millis(magnitude)),
        Some("s") => Ok(Duration::from_secs(magnitude)),
        Some("m") => Ok(Duration::from_secs(magnitude * 60)),
        Some("h") => Ok(Duration::from_secs(magnitude * 60 * 60)),
        Some("d") => Ok(Duration::from_secs(magnitude * 60 * 60 * 24)),
        _ => Err(ParseError::NotADuration),
    }
}

pub(super) fn parse_socket_addr(s: &str) -> Result<SocketAddr, ParseError> {
    match parse_addr(s)? {
        Addr::Socket(a) => Ok(a),
        _ => {
            error!("Expected IP:PORT; found: {s}");
            Err(ParseError::HostIsNotAnIpAddress)
        }
    }
}

pub(super) fn parse_socket_addrs(s: &str) -> Result<Vec<SocketAddr>, ParseError> {
    let addrs: Vec<&str> = s.split(',').collect();
    if addrs.len() > 2 {
        return Err(ParseError::TooManyAddrs);
    }
    addrs.iter().map(|s| parse_socket_addr(s)).collect()
}

pub(super) fn parse_ip_set(s: &str) -> Result<HashSet<IpAddr>, ParseError> {
    s.split(',')
        .map(|s| s.parse::<IpAddr>().map_err(Into::into))
        .collect()
}

pub(super) fn parse_addr(s: &str) -> Result<Addr, ParseError> {
    Addr::from_str(s).map_err(|e| {
        error!("Not a valid address: {s}");
        ParseError::AddrError(e)
    })
}

pub(super) fn parse_port_range_set(s: &str) -> Result<RangeInclusiveSet<u16>, ParseError> {
    let mut set = RangeInclusiveSet::new();
    if !s.is_empty() {
        for part in s.split(',') {
            let part = part.trim();
            // Ignore empty list entries
            if part.is_empty() {
                continue;
            }

            let mut parts = part.splitn(2, '-');
            let low = parts
                .next()
                .ok_or_else(|| {
                    error!("Not a valid port range: {part}");
                    ParseError::NotAPortRange
                })?
                .trim();
            let low = parse_number::<u16>(low)?;
            if let Some(high) = parts.next() {
                let high = high.trim();
                let high = parse_number::<u16>(high).map_err(|e| {
                    error!("Not a valid port range: {part}");
                    e
                })?;
                if high < low {
                    error!("Not a valid port range: {part}; {high} is greater than {low}");
                    return Err(ParseError::NotAPortRange);
                }
                set.insert(low..=high);
            } else {
                set.insert(low..=low);
            }
        }
    }
    Ok(set)
}

pub(super) fn parse_dns_suffixes(list: &str) -> Result<HashSet<dns::Suffix>, ParseError> {
    let mut suffixes = HashSet::new();
    for item in list.split(',') {
        let item = item.trim();
        if !item.is_empty() {
            let sfx = parse_dns_suffix(item)?;
            suffixes.insert(sfx);
        }
    }

    Ok(suffixes)
}

pub(super) fn parse_dns_suffix(s: &str) -> Result<dns::Suffix, ParseError> {
    if s == "." {
        return Ok(dns::Suffix::Root);
    }

    dns::Suffix::from_str(s).map_err(|_| ParseError::NotADomainSuffix)
}

pub(super) fn parse_networks(list: &str) -> Result<HashSet<IpNet>, ParseError> {
    let mut nets = HashSet::new();
    for input in list.split(',') {
        let input = input.trim();
        if !input.is_empty() {
            let net = IpNet::from_str(input).map_err(|error| {
                error!(%input, %error, "Invalid network");
                ParseError::NotANetwork
            })?;
            nets.insert(net);
        }
    }
    Ok(nets)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_unit<F: Fn(u64) -> Duration>(unit: &str, to_duration: F) {
        for v in &[0, 1, 23, 456_789] {
            let d = to_duration(*v);
            let text = format!("{}{}", v, unit);
            assert_eq!(parse_duration(&text), Ok(d), "text=\"{}\"", text);

            let text = format!(" {}{}\t", v, unit);
            assert_eq!(parse_duration(&text), Ok(d), "text=\"{}\"", text);
        }
    }

    #[test]
    fn parse_duration_unit_ms() {
        test_unit("ms", Duration::from_millis);
    }

    #[test]
    fn parse_duration_unit_s() {
        test_unit("s", Duration::from_secs);
    }

    #[test]
    fn parse_duration_unit_m() {
        test_unit("m", |v| Duration::from_secs(v * 60));
    }

    #[test]
    fn parse_duration_unit_h() {
        test_unit("h", |v| Duration::from_secs(v * 60 * 60));
    }

    #[test]
    fn parse_duration_unit_d() {
        test_unit("d", |v| Duration::from_secs(v * 60 * 60 * 24));
    }

    #[test]
    fn parse_duration_floats_invalid() {
        assert_eq!(parse_duration(".123h"), Err(ParseError::NotADuration));
        assert_eq!(parse_duration("1.23h"), Err(ParseError::NotADuration));
    }

    #[test]
    fn parse_duration_space_before_unit_invalid() {
        assert_eq!(parse_duration("1 ms"), Err(ParseError::NotADuration));
    }

    #[test]
    fn parse_duration_overflows_invalid() {
        assert!(matches!(
            parse_duration("123456789012345678901234567890ms"),
            Err(ParseError::NotAnInteger(_))
        ));
    }

    #[test]
    fn parse_duration_invalid_unit() {
        assert_eq!(parse_duration("12moons"), Err(ParseError::NotADuration));
        assert_eq!(parse_duration("12y"), Err(ParseError::NotADuration));
    }

    #[test]
    fn parse_duration_zero_without_unit() {
        assert_eq!(parse_duration("0"), Ok(Duration::from_secs(0)));
    }

    #[test]
    fn parse_duration_number_without_unit_is_invalid() {
        assert_eq!(parse_duration("1"), Err(ParseError::NotADuration));
    }

    #[test]
    fn dns_suffixes() {
        fn p(s: &str) -> Result<Vec<String>, ParseError> {
            let mut sfxs = parse_dns_suffixes(s)?
                .into_iter()
                .map(|s| format!("{}", s))
                .collect::<Vec<_>>();
            sfxs.sort();
            Ok(sfxs)
        }

        assert_eq!(p(""), Ok(vec![]), "empty string");
        assert_eq!(p(",,,"), Ok(vec![]), "empty list components are ignored");
        assert_eq!(p("."), Ok(vec![".".to_owned()]), "root is valid");
        assert_eq!(
            p("a.b.c"),
            Ok(vec!["a.b.c".to_owned()]),
            "a name without trailing dot"
        );
        assert_eq!(
            p("a.b.c."),
            Ok(vec!["a.b.c.".to_owned()]),
            "a name with a trailing dot"
        );
        assert_eq!(
            p(" a.b.c. , d.e.f. "),
            Ok(vec!["a.b.c.".to_owned(), "d.e.f.".to_owned()]),
            "whitespace is ignored"
        );
        assert_eq!(
            p("a .b.c"),
            Err(ParseError::NotADomainSuffix),
            "whitespace not allowed within a name"
        );
        assert_eq!(
            p("mUlti.CasE.nAmE"),
            Ok(vec!["multi.case.name".to_owned()]),
            "names are coerced to lowercase"
        );
    }

    #[test]
    fn ip_sets() {
        let ips = &[
            IpAddr::from([127, 0, 0, 1]),
            IpAddr::from([10, 0, 2, 42]),
            IpAddr::from([192, 168, 0, 69]),
        ];
        assert_eq!(
            parse_ip_set("127.0.0.1"),
            Ok(ips[..1].iter().cloned().collect())
        );
        assert_eq!(
            parse_ip_set("127.0.0.1,10.0.2.42"),
            Ok(ips[..2].iter().cloned().collect())
        );
        assert_eq!(
            parse_ip_set("127.0.0.1,10.0.2.42,192.168.0.69"),
            Ok(ips[..3].iter().cloned().collect())
        );
        assert!(parse_ip_set("blaah").is_err());
        assert!(parse_ip_set("10.4.0.555").is_err());
        assert!(parse_ip_set("10.4.0.3,foobar,192.168.0.69").is_err());
        assert!(parse_ip_set("10.0.1.1/24").is_err());
    }

    #[test]
    fn ranges() {
        fn set(
            ranges: impl IntoIterator<Item = std::ops::RangeInclusive<u16>>,
        ) -> Result<RangeInclusiveSet<u16>, ParseError> {
            Ok(ranges.into_iter().collect())
        }

        assert_eq!(dbg!(parse_port_range_set("1-65535")), set([1..=65535]));
        assert_eq!(
            dbg!(parse_port_range_set("1-2,42-420")),
            set([1..=2, 42..=420])
        );
        assert_eq!(
            dbg!(parse_port_range_set("1-2,42,80-420")),
            set([1..=2, 42..=42, 80..=420])
        );
        assert_eq!(
            dbg!(parse_port_range_set("1,20,30,40")),
            set([1..=1, 20..=20, 30..=30, 40..=40])
        );
        assert_eq!(
            dbg!(parse_port_range_set("40,30,20,1")),
            set([1..=1, 20..=20, 30..=30, 40..=40])
        );
        // ignores empty list entries
        assert_eq!(dbg!(parse_port_range_set("1,,,,2")), set([1..=1, 2..=2]));
        // ignores rando whitespace
        assert_eq!(
            dbg!(parse_port_range_set("1, 2,\t3- 5")),
            set([1..=1, 2..=2, 3..=5])
        );
        assert_eq!(dbg!(parse_port_range_set("1, , 2,")), set([1..=1, 2..=2]));

        // non-numeric strings
        assert!(dbg!(parse_port_range_set("asdf")).is_err());
        assert!(dbg!(parse_port_range_set("80, 443, http")).is_err());
        assert!(dbg!(parse_port_range_set("80,http-443")).is_err());

        // backwards ranges
        assert!(dbg!(parse_port_range_set("80-79")).is_err());
        assert!(dbg!(parse_port_range_set("1,2,5-2")).is_err());

        // malformed (half-open) ranges
        assert!(dbg!(parse_port_range_set("-1")).is_err());
        assert!(dbg!(parse_port_range_set("1,2-,50")).is_err());

        // not a u16
        assert!(dbg!(parse_port_range_set("69420")).is_err());
        assert!(dbg!(parse_port_range_set("1-69420")).is_err());
    }
}
