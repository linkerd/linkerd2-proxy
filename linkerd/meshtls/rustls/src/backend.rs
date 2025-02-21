#[cfg(feature = "aws-lc")]
mod aws_lc;
#[cfg(feature = "ring")]
mod ring;

#[cfg(feature = "aws-lc")]
pub use aws_lc::{default_provider, SUPPORTED_SIG_ALGS, TLS_SUPPORTED_CIPHERSUITES};
#[cfg(all(not(feature = "aws-lc"), feature = "ring"))]
pub use ring::{default_provider, SUPPORTED_SIG_ALGS, TLS_SUPPORTED_CIPHERSUITES};
#[cfg(all(not(feature = "aws-lc"), not(feature = "ring")))]
compile_error!("No rustls backend enabled. Enabled one of the \"ring\" or \"aws-lc\" features");
