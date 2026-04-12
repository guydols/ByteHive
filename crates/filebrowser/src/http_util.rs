use bytehive_core::{
    html::{PASSWORD_SHARE, PASSWORD_SHARE_ERROR, SHARE_ERROR},
    HttpResponse,
};

pub fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|p| {
        let mut parts = p.splitn(2, '=');
        let k = parts.next()?;
        if k == key {
            Some(percent_decode(parts.next().unwrap_or("")))
        } else {
            None
        }
    })
}

pub fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next().unwrap_or('0');
            let h2 = chars.next().unwrap_or('0');
            let hex = format!("{h1}{h2}");
            if let Ok(b) = u8::from_str_radix(&hex, 16) {
                out.push(b as char);
            }
        } else if c == '+' {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

pub fn share_error_page(msg: &str) -> HttpResponse {
    let html = SHARE_ERROR.replace("{msg}", msg);
    HttpResponse::ok_html(html)
}

pub fn share_password_page(token: &str, wrong: bool) -> HttpResponse {
    let err = if wrong { PASSWORD_SHARE_ERROR } else { "" };
    let html = PASSWORD_SHARE
        .replace("{token}", token)
        .replace("{err}", err);
    HttpResponse::ok_html(html)
}
