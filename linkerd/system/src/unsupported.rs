use std::{fmt, io};

#[derive(Clone, Debug)]
pub struct System(Unsupported);

#[derive(Clone, Debug)]
enum Unsupported {}

impl System {
    pub fn new() -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "procinfo not supported on this operating system",
        ))
    }
}
