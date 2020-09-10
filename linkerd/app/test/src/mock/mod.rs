pub mod connect;
pub mod refine;
pub mod resolver;
pub use tokio_test::io;

pub fn resolver<T, E>() -> resolver::Resolver<T, E>
where
    T: std::hash::Hash + Eq,
{
    resolver::Resolver::new()
}

pub fn connect() -> connect::Connect {
    connect::Connect::new()
}

pub fn io() -> io::Builder {
    io::Builder::new()
}
