pub mod app;
pub mod auth;
pub mod bus;
pub mod config;
pub mod error;
pub mod html;
pub mod http;
pub mod registry;
pub mod users;

pub use app::{App, AppContext, AppManifest};
pub use auth::Auth;
pub use bus::{BusMessage, MessageBus};
pub use config::{AppConfig, FrameworkConfig};
pub use error::CoreError;
pub use http::{ApiServer, HttpRequest, HttpResponse};
pub use registry::{AppInfo, AppRegistry, AppStatus};
pub use users::{
    ApiKey, ApiKeyInfo, AuthContext, AuthMethod, Group, Session, UserEntry, UserInfo, UserStore,
    GROUP_ADMIN, GROUP_USER,
};

pub fn timestamp_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
