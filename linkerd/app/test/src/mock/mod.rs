pub mod refine;
pub mod resolver;
use linkerd2_app_core::Error;

pub fn resolver<A, T, E>() -> resolver::Resolver<A, T, E>
where
    E: Into<Error>,
    A: std::hash::Hash + Eq,
{
    resolver::Resolver::new()
}
