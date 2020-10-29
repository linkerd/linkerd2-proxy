pub use linkerd2_app_core::{dns, profiles::*};
use tokio03::sync::watch;
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

pub fn resolver<T>() -> crate::resolver::Profiles<T>
where
    T: std::hash::Hash + Eq + std::fmt::Debug,
{
    crate::resolver::Resolver::new()
}

pub fn with_name(name: &str) -> Profile {
    use std::convert::TryFrom;
    let name = dns::Name::try_from(name.as_bytes()).expect("non-ascii characters in DNS name! 😢");
    Profile {
        name: Some(name),
        ..Default::default()
    }
}
