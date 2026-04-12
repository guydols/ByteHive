use std::fmt;

#[derive(Debug)]
pub enum CoreError {
    Io(std::io::Error),
    Config(String),
    AppAlreadyRegistered(String),
    AppNotFound(String),
    BusClosed,
    Http(String),
    App(String),
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoreError::Io(e) => write!(f, "I/O error: {e}"),
            CoreError::Config(s) => write!(f, "config error: {s}"),
            CoreError::AppAlreadyRegistered(n) => write!(f, "app already registered: {n}"),
            CoreError::AppNotFound(n) => write!(f, "app not found: {n}"),
            CoreError::BusClosed => write!(f, "message bus closed"),
            CoreError::Http(s) => write!(f, "HTTP error: {s}"),
            CoreError::App(s) => write!(f, "app error: {s}"),
        }
    }
}

impl std::error::Error for CoreError {}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        CoreError::Io(e)
    }
}
