use bytehive_core::error::CoreError;

#[test]
fn display_io() {
    let e = CoreError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"));
    assert!(e.to_string().starts_with("I/O error:"));
}

#[test]
fn display_config() {
    let e = CoreError::Config("bad toml".into());
    assert_eq!(e.to_string(), "config error: bad toml");
}

#[test]
fn display_app_already_registered() {
    let e = CoreError::AppAlreadyRegistered("myapp".into());
    assert_eq!(e.to_string(), "app already registered: myapp");
}

#[test]
fn display_app_not_found() {
    let e = CoreError::AppNotFound("ghost".into());
    assert_eq!(e.to_string(), "app not found: ghost");
}

#[test]
fn display_bus_closed() {
    assert_eq!(CoreError::BusClosed.to_string(), "message bus closed");
}

#[test]
fn display_http() {
    let e = CoreError::Http("connect refused".into());
    assert_eq!(e.to_string(), "HTTP error: connect refused");
}

#[test]
fn display_app_error() {
    let e = CoreError::App("crashed".into());
    assert_eq!(e.to_string(), "app error: crashed");
}

#[test]
fn from_io_error() {
    let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
    let core_err = CoreError::from(io_err);
    assert!(matches!(core_err, CoreError::Io(_)));
}
