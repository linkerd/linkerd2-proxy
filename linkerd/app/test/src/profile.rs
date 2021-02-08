pub use linkerd_app_core::{dns, profiles::*};
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

pub fn with_name(name: &str) -> Profile {
    use std::str::FromStr;
    let name = dns::Name::from_str(name).expect("non-ascii characters in DNS name! 😢");
    Profile {
        name: Some(name),
        ..Default::default()
    }
}
