use std::{io, path::PathBuf};

#[derive(Clone, Debug)]
pub struct TokenSource(PathBuf);

// === impl TokenSource ===

impl TokenSource {
    pub fn if_nonempty_file(p: impl Into<PathBuf>) -> io::Result<Self> {
        let ts = TokenSource(p.into());
        ts.load()?;
        Ok(ts)
    }

    pub fn load(&self) -> io::Result<Vec<u8>> {
        let t = std::fs::read(&self.0)?;

        if t.is_empty() {
            return Err(io::Error::new(io::ErrorKind::Other, "token is empty"));
        }

        Ok(t)
    }
}
