use std::fmt;

#[derive(Debug, Clone)]
pub struct SshuError(pub String);

impl SshuError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl fmt::Display for SshuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SshuError {}

pub type Result<T> = std::result::Result<T, SshuError>;
