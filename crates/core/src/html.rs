pub const DODECAHEDRON_SVG: &str = r#"<svg class="dode" viewBox="0 0 40 40" width="22" height="22"
  fill="none" stroke="currentColor" stroke-width="1.7"
  stroke-linejoin="round" stroke-linecap="round" aria-hidden="true">
  <polygon points="20,3 36.2,14.7 30,33.8 10,33.8 3.8,14.7"/>
  <polygon points="25.3,12.7 28.6,22.8 20,29 11.4,22.8 14.7,12.7" opacity=".65"/>
  <line x1="20"   y1="3"    x2="25.3" y2="12.7"/>
  <line x1="36.2" y1="14.7" x2="28.6" y2="22.8"/>
  <line x1="30"   y1="33.8" x2="20"   y2="29"/>
  <line x1="10"   y1="33.8" x2="11.4" y2="22.8"/>
  <line x1="3.8"  y1="14.7" x2="14.7" y2="12.7"/>
</svg>"#;

pub const FLAT_KIT_CSS: &str = include_str!("../assets/shared.css");

pub const SETUP_HTML: &str = include_str!("../assets/setup.html");

pub const PORTAL_HTML: &str = include_str!("../assets/portal.html");

pub const ADMIN_DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");

pub const FILEBROWSER_HTML: &str = include_str!("../assets/filebrowser.html");

pub const SHARE_ERROR: &str = include_str!("../assets/share_error.html");

pub const PASSWORD_SHARE_ERROR: &str = r#"<p class="err">Incorrect password — try again.</p>"#;

pub const PASSWORD_SHARE: &str = include_str!("../assets/password_share.html");
