use crate::auth::Auth;
use crate::bus::{BusMessage, MessageBus};
use crate::html::{ADMIN_DASHBOARD_HTML, PORTAL_HTML, SETUP_HTML};
use crate::registry::AppRegistry;
use crate::users::{AuthContext, AuthMethod, Group, UserEntry, UserStore};

use axum::{
    body::Body,
    extract::{OriginalUri, Path, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post, put},
    Json, Router,
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tower_http::cors::{Any, CorsLayer};

const BYTEHIVE_ICON_SVG: &str = include_str!("../assets/bytehive-icon.svg");
const BYTEHIVE_LOGO_FULL_SVG: &str = include_str!("../assets/bytehive-logo-full.svg");

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub auth: Option<AuthContext>,
}

impl HttpRequest {
    pub fn json(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn ok_json(value: impl serde::Serialize) -> Self {
        let body = serde_json::to_vec_pretty(&value).unwrap_or_default();
        Self {
            status: 200,
            content_type: "application/json; charset=utf-8".into(),
            headers: HashMap::new(),
            body,
        }
    }
    pub fn ok_html(html: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8".into(),
            headers: HashMap::new(),
            body: html.into(),
        }
    }
    pub fn ok_text(text: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "text/plain; charset=utf-8".into(),
            headers: HashMap::new(),
            body: text.into().into_bytes(),
        }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        let body = json!({ "error": msg.into() });
        Self {
            status: 404,
            content_type: "application/json".into(),
            headers: HashMap::new(),
            body: serde_json::to_vec(&body).unwrap_or_default(),
        }
    }
    pub fn unauthorized() -> Self {
        Self {
            status: 401,
            content_type: "application/json".into(),
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({"error":"unauthorized"})).unwrap_or_default(),
        }
    }
    pub fn forbidden() -> Self {
        Self {
            status: 403,
            content_type: "application/json".into(),
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({"error":"forbidden — admin role required"}))
                .unwrap_or_default(),
        }
    }
    pub fn bad_request(msg: impl Into<String>) -> Self {
        let body = json!({ "error": msg.into() });
        Self {
            status: 400,
            content_type: "application/json".into(),
            headers: HashMap::new(),
            body: serde_json::to_vec(&body).unwrap_or_default(),
        }
    }
    pub fn internal_error(msg: impl Into<String>) -> Self {
        let body = json!({ "error": msg.into() });
        Self {
            status: 500,
            content_type: "application/json".into(),
            headers: HashMap::new(),
            body: serde_json::to_vec(&body).unwrap_or_default(),
        }
    }
    pub fn with_header(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.headers.insert(key.into(), val.into());
        self
    }
}

#[derive(Clone)]
pub struct ApiState {
    pub registry: Arc<AppRegistry>,
    pub bus: Arc<MessageBus>,
    pub auth: Arc<Auth>,
    pub users: Arc<UserStore>,
    pub web_root: String,
}

pub struct ApiServer {
    addr: String,
    registry: Arc<AppRegistry>,
    bus: Arc<MessageBus>,
    auth: Arc<Auth>,
    users: Arc<UserStore>,
    web_root: String,
}

impl ApiServer {
    pub fn new(
        addr: impl Into<String>,
        registry: Arc<AppRegistry>,
        bus: Arc<MessageBus>,
        auth: Arc<Auth>,
        users: Arc<UserStore>,
        web_root: impl Into<String>,
    ) -> Self {
        Self {
            addr: addr.into(),
            registry,
            bus,
            auth,
            users,
            web_root: web_root.into(),
        }
    }

    pub fn start(self) -> Result<std::thread::JoinHandle<()>, std::io::Error> {
        let addr = self.addr.clone();
        let state = ApiState {
            registry: self.registry,
            bus: self.bus,
            auth: self.auth,
            users: self.users,
            web_root: self.web_root,
        };
        std::thread::Builder::new()
            .name("http-api".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(async move {
                    let router = build_router(state);
                    let listener = tokio::net::TcpListener::bind(&addr)
                        .await
                        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));
                    log::info!("ByteHive portal -> http://{addr}/");
                    log::info!("ByteHive admin  -> http://{addr}/admin");
                    axum::serve(listener, router).await.expect("axum serve");
                });
            })
    }
}

fn build_router(state: ApiState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let admin_ops = Router::new()
        .route("/users", get(list_users_handler))
        .route("/users", post(create_user_handler))
        .route("/users/:username", put(update_user_handler))
        .route("/users/:username", delete(delete_user_handler))
        .route("/groups", get(list_groups_handler))
        .route("/groups", post(create_group_handler))
        .route("/groups/:name", delete(delete_group_handler))
        .route("/groups/:name/members/:username", post(add_member_handler))
        .route(
            "/groups/:name/members/:username",
            delete(remove_member_handler),
        )
        .route("/apikeys", get(list_apikeys_handler))
        .route("/apikeys", post(create_apikey_handler))
        .route("/apikeys/:name", delete(revoke_apikey_handler))
        .route("/config/export", get(export_config_handler))
        .route("/status", get(core_status_handler))
        .route("/apps", get(list_apps_handler))
        .route("/apps/:name", get(app_info_handler))
        .route("/apps/:name/config", put(update_config_handler))
        .route("/apps/:name/start", post(start_app_handler))
        .route("/apps/:name/stop", post(stop_app_handler))
        .route("/apps/:name/restart", post(restart_app_handler))
        .route("/events", get(events_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_middleware,
        ));

    let authenticated_auth = Router::new()
        .route("/me", get(me_handler))
        .route("/logout", post(logout_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    let public_auth = Router::new()
        .route("/login", post(login_handler))
        .route("/setup", post(setup_api_handler));

    let api_proxy = Router::new()
        .fallback(proxy_handler)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    let apps_proxy = Router::new()
        .fallback(proxy_handler)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    let admin_page =
        Router::new()
            .route("/", get(admin_handler))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                admin_middleware,
            ));

    Router::new()
        .route("/setup", get(setup_page_handler))
        .route("/", get(portal_handler))
        .route("/bytehive-icon.svg", get(bytehive_icon_handler))
        .route("/bytehive-logo-full.svg", get(bytehive_logo_full_handler))
        .route("/web/*path", get(static_handler))
        .route("/s/:token", get(share_handler).post(share_handler))
        .nest("/api/auth", public_auth.merge(authenticated_auth))
        .nest("/api/core", admin_ops)
        .nest("/admin", admin_page)
        .nest("/apps", apps_proxy)
        .nest("/api", api_proxy)
        .with_state(state)
        .layer(cors)
}

async fn auth_middleware(State(s): State<ApiState>, mut req: Request, next: Next) -> Response {
    if s.users.needs_setup() {
        return (
            StatusCode::FOUND,
            [(axum::http::header::LOCATION, "/setup")],
        )
            .into_response();
    }

    if let Some(ctx) = resolve_auth_context(&s, &req) {
        if let AuthMethod::Session = ctx.method {
            if let Some(tok) = extract_bearer_or_cookie(&req, &s) {
                s.users.refresh(&tok);
            }
        }
        inject_auth_context(&mut req, &ctx);
        req.extensions_mut().insert(ctx);
        return next.run(req).await;
    }

    let path = req.uri().path().to_string();
    if path.starts_with("/apps/") {
        (
            StatusCode::FOUND,
            [(
                axum::http::header::LOCATION,
                format!("/?redirect={}", urlencoded(&path)),
            )],
        )
            .into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"authentication required"})),
        )
            .into_response()
    }
}

async fn admin_middleware(State(s): State<ApiState>, mut req: Request, next: Next) -> Response {
    if s.users.needs_setup() {
        return (
            StatusCode::FOUND,
            [(axum::http::header::LOCATION, "/setup")],
        )
            .into_response();
    }

    if let Some(ctx) = resolve_auth_context(&s, &req) {
        if ctx.is_admin() {
            if let AuthMethod::Session = ctx.method {
                if let Some(tok) = extract_bearer_or_cookie(&req, &s) {
                    s.users.refresh(&tok);
                }
            }
            inject_auth_context(&mut req, &ctx);
            req.extensions_mut().insert(ctx);
            return next.run(req).await;
        }

        let path = req.uri().path().to_string();
        if path == "/admin" || path == "/admin/" {
            return (
                StatusCode::FOUND,
                [(axum::http::header::LOCATION, "/?redirect=/admin")],
            )
                .into_response();
        }
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"admin role required"})),
        )
            .into_response();
    }

    let path = req.uri().path().to_string();
    if path == "/admin" || path == "/admin/" {
        return (
            StatusCode::FOUND,
            [(
                axum::http::header::LOCATION,
                format!("/?redirect={}", urlencoded(&path)),
            )],
        )
            .into_response();
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error":"authentication required"})),
    )
        .into_response()
}

fn resolve_auth_context(s: &ApiState, req: &Request) -> Option<AuthContext> {
    let cookie_tok = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| parse_cookie(h, "cc_session"))
        .map(|t| t.to_string());

    let bearer_tok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").map(|t| t.trim().to_string()))
        .or_else(|| {
            req.uri()
                .query()
                .unwrap_or("")
                .split('&')
                .find_map(|p| p.strip_prefix("token=").map(|v| v.to_string()))
        });

    if let Some(tok) = &cookie_tok {
        if let Some(sess) = s.users.validate(tok) {
            let groups = s.users.groups_for_user(&sess.username);
            return Some(AuthContext {
                username: sess.username.clone(),
                display_name: sess.display_name.clone(),
                groups,
                method: AuthMethod::Session,
            });
        }
    }

    if let Some(tok) = &bearer_tok {
        return s.users.authenticate_credential(tok);
    }

    None
}

fn extract_bearer_or_cookie(req: &Request, s: &ApiState) -> Option<String> {
    let _ = s;
    req.headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| parse_cookie(h, "cc_session"))
        .map(|t| t.to_string())
        .or_else(|| {
            req.headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer ").map(|t| t.trim().to_string()))
        })
}

fn inject_auth_context(req: &mut Request, ctx: &AuthContext) {
    if let Ok(v) = ctx.username.parse::<axum::http::HeaderValue>() {
        req.headers_mut().insert("x-bytehive-user", v);
    }
    let role_str = if ctx.is_admin() {
        "admin"
    } else if ctx.can_write() {
        "user"
    } else {
        "readonly"
    };
    if let Ok(v) = role_str.parse::<axum::http::HeaderValue>() {
        req.headers_mut().insert("x-bytehive-role", v);
    }
}

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

async fn login_handler(State(s): State<ApiState>, Json(body): Json<LoginBody>) -> Response {
    let users = s.users.clone();
    let result = tokio::task::spawn_blocking(move || users.login(&body.username, &body.password))
        .await
        .unwrap_or_else(|e| {
            log::error!("login task panicked: {e}");
            None
        });

    match result {
        Some(sess) => {
            let groups = s.users.groups_for_user(&sess.username);
            let cookie = format!(
                "cc_session={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
                sess.token,
                sess.ttl_secs()
            );
            (
                StatusCode::OK,
                [(axum::http::header::SET_COOKIE, cookie)],
                Json(json!({
                    "ok": true,
                    "user": {
                        "username":     sess.username,
                        "display_name": sess.display_name,
                        "groups":       groups,
                    }
                })),
            )
                .into_response()
        }
        None => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok":false,"error":"invalid username or password"})),
        )
            .into_response(),
    }
}

async fn logout_handler(State(s): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(tok) = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| parse_cookie(h, "cc_session"))
    {
        s.users.logout(tok);
    }
    (
        StatusCode::OK,
        [(
            axum::http::header::SET_COOKIE,
            "cc_session=; Path=/; HttpOnly; Max-Age=0",
        )],
        Json(json!({"ok":true})),
    )
}

async fn me_handler(State(s): State<ApiState>, headers: HeaderMap) -> Response {
    if s.users.needs_setup() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok":false,"error":"setup required"})),
        )
            .into_response();
    }
    let cookie_tok = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| parse_cookie(h, "cc_session"))
        .map(|t| t.to_string());
    let bearer_tok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").map(|t| t.trim().to_string()));

    let session = cookie_tok
        .as_deref()
        .and_then(|t| s.users.validate(t))
        .or_else(|| bearer_tok.as_deref().and_then(|t| s.users.validate(t)));

    match session {
        Some(sess) => {
            let groups = s.users.groups_for_user(&sess.username);
            Json(json!({
                "ok": true,
                "user": {
                    "username":     sess.username,
                    "display_name": sess.display_name,
                    "groups":       groups,
                }
            }))
            .into_response()
        }
        None => (StatusCode::UNAUTHORIZED, Json(json!({"ok":false}))).into_response(),
    }
}

async fn portal_handler(State(s): State<ApiState>) -> Response {
    if s.users.needs_setup() {
        return (
            StatusCode::FOUND,
            [(axum::http::header::LOCATION, "/setup")],
        )
            .into_response();
    }
    axum::response::Html(PORTAL_HTML).into_response()
}

async fn admin_handler() -> impl IntoResponse {
    axum::response::Html(ADMIN_DASHBOARD_HTML)
}

async fn setup_page_handler(State(s): State<ApiState>) -> Response {
    if !s.users.needs_setup() {
        return (StatusCode::FOUND, [(axum::http::header::LOCATION, "/")]).into_response();
    }
    axum::response::Html(SETUP_HTML).into_response()
}

#[derive(Deserialize)]
struct SetupBody {
    password: String,
}

async fn setup_api_handler(State(s): State<ApiState>, Json(body): Json<SetupBody>) -> Response {
    if !s.users.needs_setup() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"ok":false,"error":"setup has already been completed"})),
        )
            .into_response();
    }

    if let Err(e) = s.users.complete_setup(&body.password) {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok":false,"error":e}))).into_response();
    }

    match s.users.login("admin", &body.password) {
        Some(sess) => {
            let groups = s.users.groups_for_user(&sess.username);
            let cookie = format!(
                "cc_session={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
                sess.token,
                sess.ttl_secs()
            );
            (
                StatusCode::OK,
                [(axum::http::header::SET_COOKIE, cookie)],
                Json(json!({
                    "ok": true,
                    "user": {
                        "username":     sess.username,
                        "display_name": sess.display_name,
                        "groups":       groups,
                    }
                })),
            )
                .into_response()
        }

        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok":false,"error":"account created but auto-login failed — please log in manually"})),
        )
            .into_response(),
    }
}

async fn bytehive_icon_handler() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        BYTEHIVE_ICON_SVG,
    )
}

async fn bytehive_logo_full_handler() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        BYTEHIVE_LOGO_FULL_SVG,
    )
}

async fn static_handler(State(s): State<ApiState>, Path(rel): Path<String>) -> impl IntoResponse {
    if s.web_root.is_empty() {
        return (StatusCode::NOT_FOUND, "no web_root configured").into_response();
    }
    let full = std::path::Path::new(&s.web_root).join(&rel);
    match std::fs::read(&full) {
        Ok(bytes) => (
            [(axum::http::header::CONTENT_TYPE, mime_for_path(&rel))],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, format!("not found: {rel}")).into_response(),
    }
}

async fn core_status_handler(State(s): State<ApiState>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "framework": env!("CARGO_PKG_VERSION"),
        "apps": s.registry.all_app_infos(),
    }))
}

async fn list_apps_handler(State(s): State<ApiState>) -> impl IntoResponse {
    Json(json!({"apps": s.registry.all_app_infos()}))
}

async fn app_info_handler(State(s): State<ApiState>, Path(name): Path<String>) -> Response {
    match s.registry.app_info(&name) {
        Some(i) => Json(i).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("'{name}' not found")})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateConfigBody {
    toml: String,
}

async fn update_config_handler(
    State(s): State<ApiState>,
    Path(name): Path<String>,
    Json(b): Json<UpdateConfigBody>,
) -> Response {
    match s.registry.update_config(&name, &b.toml) {
        Ok(()) => Json(json!({"ok":true,"message":"Saved. Restart to apply."})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":e.to_string()})),
        )
            .into_response(),
    }
}

async fn start_app_handler(State(s): State<ApiState>, Path(name): Path<String>) -> Response {
    let reg = Arc::clone(&s.registry);
    match tokio::task::spawn_blocking(move || reg.start_app(&name)).await {
        Ok(Ok(())) => Json(json!({"ok":true})).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":e.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"task panicked"})),
        )
            .into_response(),
    }
}

async fn stop_app_handler(State(s): State<ApiState>, Path(name): Path<String>) -> Response {
    let reg = Arc::clone(&s.registry);
    match tokio::task::spawn_blocking(move || reg.stop_app(&name)).await {
        Ok(Ok(())) => Json(json!({"ok":true})).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":e.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"task panicked"})),
        )
            .into_response(),
    }
}

async fn restart_app_handler(State(s): State<ApiState>, Path(name): Path<String>) -> Response {
    let reg = Arc::clone(&s.registry);
    match tokio::task::spawn_blocking(move || reg.restart_app(&name)).await {
        Ok(Ok(())) => Json(json!({"ok":true})).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":e.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"task panicked"})),
        )
            .into_response(),
    }
}

async fn events_handler(
    State(s): State<ApiState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let rx = s.bus.sub("*");
    let (tx, rx_tok) = tokio::sync::mpsc::unbounded_channel::<Arc<BusMessage>>();
    std::thread::Builder::new()
        .name("sse-bridge".into())
        .spawn(move || {
            for msg in rx {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        })
        .ok();
    let stream = UnboundedReceiverStream::new(rx_tok).map(|msg: Arc<BusMessage>| {
        let data = json!({
            "id": msg.id, "source": msg.source,
            "topic": msg.topic, "payload": msg.payload,
            "ts": msg.timestamp_ms,
        })
        .to_string();
        Ok::<_, Infallible>(Event::default().data(data))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn list_users_handler(State(s): State<ApiState>) -> impl IntoResponse {
    let users: Vec<serde_json::Value> = s
        .users
        .list_users()
        .into_iter()
        .map(|u| {
            json!({
                "username":     u.username,
                "display_name": u.display_name,
                "groups":       s.users.groups_for_user(&u.username),
            })
        })
        .collect();
    Json(json!({"users": users}))
}

#[derive(Deserialize)]
struct CreateUserBody {
    username: String,
    password: String,
    #[serde(default)]
    display_name: String,

    #[serde(default)]
    groups: Vec<String>,
}

async fn create_user_handler(State(s): State<ApiState>, Json(b): Json<CreateUserBody>) -> Response {
    let entry = UserEntry {
        username: b.username.clone(),
        password_hash: UserStore::hash_password(&b.password),
        display_name: b.display_name,
    };
    if let Err(e) = s.users.add_user(entry) {
        return (StatusCode::CONFLICT, Json(json!({"error":e}))).into_response();
    }

    let target_groups = if b.groups.is_empty() {
        vec!["user".to_string()]
    } else {
        b.groups
    };
    for group in &target_groups {
        if let Err(e) = s.users.add_member_to_group(group, &b.username) {
            log::warn!("create_user: add to group '{group}': {e}");
        }
    }
    Json(json!({"ok":true})).into_response()
}

#[derive(Deserialize)]
pub struct UpdateUserBody {
    pub display_name: Option<String>,
    pub password: Option<String>,
}

async fn update_user_handler(
    State(s): State<ApiState>,
    Path(username): Path<String>,
    Json(b): Json<UpdateUserBody>,
) -> Response {
    match s
        .users
        .update_user(&username, b.display_name, b.password.as_deref())
    {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({"error":e}))).into_response(),
    }
}

async fn delete_user_handler(State(s): State<ApiState>, Path(username): Path<String>) -> Response {
    match s.users.remove_user(&username) {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({"error":e}))).into_response(),
    }
}

async fn list_groups_handler(State(s): State<ApiState>) -> impl IntoResponse {
    Json(json!({"groups": s.users.list_groups()}))
}

#[derive(Deserialize)]
struct CreateGroupBody {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    members: Vec<String>,
}

async fn create_group_handler(
    State(s): State<ApiState>,
    Json(b): Json<CreateGroupBody>,
) -> Response {
    let group = Group {
        name: b.name,
        description: b.description,
        members: b.members,
    };
    match s.users.add_group(group) {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(json!({"error":e}))).into_response(),
    }
}

async fn delete_group_handler(State(s): State<ApiState>, Path(name): Path<String>) -> Response {
    match s.users.remove_group(&name) {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({"error":e}))).into_response(),
    }
}

async fn add_member_handler(
    State(s): State<ApiState>,
    Path((group, username)): Path<(String, String)>,
) -> Response {
    match s.users.add_member_to_group(&group, &username) {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({"error":e}))).into_response(),
    }
}

async fn remove_member_handler(
    State(s): State<ApiState>,
    Path((group, username)): Path<(String, String)>,
) -> Response {
    match s.users.remove_member_from_group(&group, &username) {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({"error":e}))).into_response(),
    }
}

async fn list_apikeys_handler(State(s): State<ApiState>) -> impl IntoResponse {
    Json(json!({"api_keys": s.users.list_api_keys()}))
}

#[derive(Deserialize)]
struct CreateApiKeyBody {
    name: String,
    #[serde(default)]
    as_user: String,
    expires_ms: Option<u64>,
}

async fn create_apikey_handler(
    State(s): State<ApiState>,
    Json(b): Json<CreateApiKeyBody>,
) -> Response {
    match s.users.create_api_key(b.name, b.as_user, b.expires_ms) {
        Ok(key) => Json(
            json!({"ok":true,"key":key,"note":"Store this value — it will not be shown again."}),
        )
        .into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(json!({"error":e}))).into_response(),
    }
}

async fn revoke_apikey_handler(State(s): State<ApiState>, Path(name): Path<String>) -> Response {
    match s.users.revoke_api_key(&name) {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({"error":e}))).into_response(),
    }
}

async fn export_config_handler(State(s): State<ApiState>) -> impl IntoResponse {
    let mut out = String::new();
    for u in s.users.list_users() {
        out.push_str(&format!(
            "[[users]]\nusername = {:?}\npassword_hash = {:?}\ndisplay_name = {:?}\n\n",
            u.username, u.password_hash, u.display_name
        ));
    }
    for g in s.users.list_groups() {
        let members: Vec<String> = g.members.iter().map(|m| format!("{:?}", m)).collect();
        out.push_str(&format!(
            "[[groups]]\nname = {:?}\ndescription = {:?}\nmembers = [{}]\n\n",
            g.name,
            g.description,
            members.join(", ")
        ));
    }
    for k in s.users.list_api_keys() {
        out.push_str(&format!(
            "[[api_keys]]\nname = {:?}\nas_user = {:?}\ncreated_at = {}\n",
            k.name, k.as_user, k.created_at
        ));
        if let Some(exp) = k.expires_ms {
            out.push_str(&format!("expires_ms = {exp}\n"));
        }
        out.push_str("# key value omitted for security — manage via admin panel\n\n");
    }
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        out,
    )
}

async fn share_handler(
    State(s): State<ApiState>,
    Path(token): Path<String>,
    req: Request,
) -> Response {
    dispatch_to_registry(s, req, Some(format!("/api/filebrowser/s/{token}")), None).await
}

async fn proxy_handler(
    State(s): State<ApiState>,
    OriginalUri(original_uri): OriginalUri,
    req: Request,
) -> Response {
    let full_path = original_uri.path().to_string();

    let auth_ctx = req.extensions().get::<AuthContext>().cloned();
    dispatch_to_registry(s, req, Some(full_path), auth_ctx).await
}

async fn dispatch_to_registry(
    s: ApiState,
    req: Request,
    path_override: Option<String>,
    auth: Option<AuthContext>,
) -> Response {
    let method = req.method().to_string();
    let uri = req.uri().clone();
    let path = path_override.unwrap_or_else(|| uri.path().to_string());
    let query = uri.query().unwrap_or("").to_string();
    let mut headers = HashMap::new();
    for (k, v) in req.headers() {
        if let Ok(v) = v.to_str() {
            headers.insert(k.to_string().to_lowercase(), v.to_string());
        }
    }
    let body = axum::body::to_bytes(req.into_body(), 512 * 1024 * 1024)
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();

    let resp = s.registry.route_http(&HttpRequest {
        method,
        path,
        query,
        headers,
        body,
        auth,
    });

    let mut builder = Response::builder()
        .status(resp.status)
        .header("content-type", &resp.content_type);
    for (k, v) in &resp.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    builder
        .body(Body::from(resp.body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

pub fn parse_cookie<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    for part in header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(name) {
            let val = rest.trim_start_matches('=').trim();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

pub fn urlencoded(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' => c.to_string(),
            c => format!("%{:02X}", c as u32),
        })
        .collect()
}

pub fn mime_for_path(path: &str) -> &'static str {
    let path_lower = path.to_lowercase();

    if path_lower.ends_with(".html") || path_lower.ends_with(".htm") {
        "text/html; charset=utf-8"
    } else if path_lower.ends_with(".js") || path_lower.ends_with(".mjs") {
        "application/javascript"
    } else if path_lower.ends_with(".css") {
        "text/css"
    } else if path_lower.ends_with(".json") {
        "application/json"
    } else if path_lower.ends_with(".txt") || path_lower.ends_with(".text") {
        "text/plain"
    } else if path_lower.ends_with(".png") {
        "image/png"
    } else if path_lower.ends_with(".jpg") || path_lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if path_lower.ends_with(".gif") {
        "image/gif"
    } else if path_lower.ends_with(".webp") {
        "image/webp"
    } else if path_lower.ends_with(".svg") {
        "image/svg+xml"
    } else if path_lower.ends_with(".ico") {
        "image/x-icon"
    } else if path_lower.ends_with(".woff") {
        "font/woff"
    } else if path_lower.ends_with(".woff2") {
        "font/woff2"
    } else if path_lower.ends_with(".ttf") {
        "font/ttf"
    } else if path_lower.ends_with(".otf") {
        "font/otf"
    } else if path_lower.ends_with(".mp4") || path_lower.ends_with(".m4v") {
        "video/mp4"
    } else if path_lower.ends_with(".webm") {
        "video/webm"
    } else if path_lower.ends_with(".mp3") {
        "audio/mpeg"
    } else if path_lower.ends_with(".wav") {
        "audio/wav"
    } else if path_lower.ends_with(".ogg") || path_lower.ends_with(".oga") {
        "audio/ogg"
    } else if path_lower.ends_with(".pdf") {
        "application/pdf"
    } else if path_lower.ends_with(".zip") {
        "application/zip"
    } else if path_lower.ends_with(".tar.gz") || path_lower.ends_with(".tgz") {
        "application/gzip"
    } else if path_lower.ends_with(".gz") || path_lower.ends_with(".gzip") {
        "application/gzip"
    } else if path_lower.ends_with(".tar") {
        "application/x-tar"
    } else if path_lower.ends_with(".rar") {
        "application/x-rar-compressed"
    } else if path_lower.ends_with(".doc") {
        "application/msword"
    } else if path_lower.ends_with(".docx") {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    } else if path_lower.ends_with(".xls") {
        "application/vnd.ms-excel"
    } else if path_lower.ends_with(".xlsx") {
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    } else {
        "application/octet-stream"
    }
}
