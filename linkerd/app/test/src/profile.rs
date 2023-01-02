pub use linkerd_app_core::{profiles::*, NameAddr};
use std::str::FromStr;
pub use tokio::sync::watch;
pub use watch::channel;

pub type Sender = watch::Sender<Profile>;

/// Returns a `Receiver` that contains only the initial value, and closes when
/// the receiver is dropped.
pub fn only(profile: Profile) -> Receiver {
    let (tx, rx) = channel(profile);
    tokio::spawn(async move { tx.closed().await });
    rx.into()
}

pub fn resolver() -> crate::resolver::Profiles {
    crate::resolver::Resolver::default()
}

pub fn only_with_addr(addr: &str) -> Receiver {
    only(with_addr(addr))
}

pub fn empty() -> Profile {
    Profile {
        addr: None,
        http_routes: std::iter::empty().collect(),
        tcp_routes: std::iter::empty().collect(),
        target_addrs: Default::default(),
        endpoint: None,
        opaque_protocol: false,
    }
}

pub fn with_addr(addr: &str) -> Profile {
    let na = NameAddr::from_str(addr).expect("Invalid name:port");
    let targets = std::iter::once(Target {
        addr: na.clone(),
        weight: 1,
    })
    .collect::<Targets>();
    let target_addrs = std::iter::once(na.clone()).collect();
    Profile {
        addr: Some(LogicalAddr(na)),
        http_routes: std::iter::once((
            http::RequestMatch::default(),
            http::Route::new(std::iter::empty(), Vec::new(), targets.clone()),
        ))
        .collect(),
        tcp_routes: std::iter::once((tcp::RequestMatch::default(), tcp::Route::new(targets)))
            .collect(),
        target_addrs,
        endpoint: None,
        opaque_protocol: false,
    }
}
