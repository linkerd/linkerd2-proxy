pub use linkerd_app_core::{profiles::*, NameAddr};
use std::str::FromStr;
pub use tokio::sync::watch;
pub use watch::channel;

pub type Sender = watch::Sender<Profile>;

/// Returns a `Receiver` that contains only the default profile, and closes when
/// the receiver is dropped.
pub fn only_default() -> Receiver {
    only(Profile::default())
}

/// Returns a `Receiver` that contains only the initial value, and closes when
/// the receiver is dropped.
pub fn only(profile: Profile) -> Receiver {
    let (tx, rx) = channel(profile);
    tokio::spawn(async move { tx.closed().await });
    rx
}

pub fn resolver() -> crate::resolver::Profiles {
    crate::resolver::Resolver::default()
}

pub fn only_with_addr(addr: &str) -> Receiver {
    only(with_addr(addr))
}

pub fn with_addr(addr: &str) -> Profile {
    let na = NameAddr::from_str(addr).expect("Invalid name:port");
    Profile {
        addr: Some(LogicalAddr(na)),
        ..Default::default()
    }
}
