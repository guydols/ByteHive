use crate::app::FileBrowserApp;
use crate::crypto::now_ms;
use crate::file_type::{is_text_file, monaco_language, sniff_is_text};
use crate::fs_util::{copy_dir_all, mime_for_file, resolve, search_dir, zip_directory};
use crate::http_util::{query_param, share_error_page, share_password_page};
use crate::share::Share;
use bytehive_core::{HttpRequest, HttpResponse};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::UNIX_EPOCH;
use uuid::Uuid;

impl FileBrowserApp {
    pub fn handle_status(&self) -> HttpResponse {
        let guard = self.inner.read();
        match guard.as_ref() {
            None => HttpResponse::internal_error("not running"),
            Some(inner) => {
                let share_count = self
                    .shares
                    .read()
                    .values()
                    .filter(|s| !s.is_expired())
                    .count();
                HttpResponse::ok_json(json!({
                    "root":          inner.root.display().to_string(),
                    "share_count":   share_count,
                    "max_upload_mb": inner.max_upload_bytes / (1024 * 1024),
                    "allow_delete":  inner.allow_delete,
                }))
            }
        }
    }

    pub fn handle_ls(&self, req: &HttpRequest) -> HttpResponse {
        let root = match self.root() {
            Some(r) => r,
            None => return HttpResponse::internal_error("not running"),
        };

        let rel = query_param(&req.query, "path").unwrap_or_default();
        let abs = match resolve(&root, &rel) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };

        if !abs.is_dir() {
            return HttpResponse::bad_request(format!("not a directory: {rel}"));
        }

        let mut entries: Vec<Value> = Vec::new();
        match std::fs::read_dir(&abs) {
            Err(e) => return HttpResponse::internal_error(e.to_string()),
            Ok(rd) => {
                for entry in rd.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    // Hide the internal .bh_filesync folder from directory listings
                    if name == ".bh_filesync" {
                        continue;
                    }
                    let meta = entry.metadata().ok();
                    let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                    let mtime = meta
                        .as_ref()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let path = if rel.is_empty() {
                        name.clone()
                    } else {
                        format!("{rel}/{name}")
                    };
                    entries.push(json!({
                        "name":   name,
                        "path":   path,
                        "is_dir": is_dir,
                        "size":   size,
                        "mtime":  mtime,
                        "ext":    crate::fs_util::extension(&name),
                    }));
                }
            }
        }

        entries.sort_by(|a, b| {
            let ad = a["is_dir"].as_bool().unwrap_or(false);
            let bd = b["is_dir"].as_bool().unwrap_or(false);
            bd.cmp(&ad).then_with(|| {
                a["name"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["name"].as_str().unwrap_or(""))
            })
        });

        HttpResponse::ok_json(json!({ "path": rel, "entries": entries }))
    }

    pub fn handle_download(&self, req: &HttpRequest) -> HttpResponse {
        let root = match self.root() {
            Some(r) => r,
            None => return HttpResponse::internal_error("not running"),
        };

        let rel = match query_param(&req.query, "path") {
            Some(p) => p,
            None => return HttpResponse::bad_request("path required"),
        };

        let abs = match resolve(&root, &rel) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };

        if abs.is_dir() {
            return zip_directory(&abs, &rel);
        }

        match std::fs::read(&abs) {
            Err(e) => HttpResponse::internal_error(e.to_string()),
            Ok(bytes) => {
                let ct = mime_for_file(&rel);
                let name = abs
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".to_string());
                HttpResponse {
                    status: 200,
                    content_type: ct.to_string(),
                    headers: {
                        let mut h = HashMap::new();
                        h.insert(
                            "content-disposition".into(),
                            format!("attachment; filename=\"{}\"", name),
                        );
                        h
                    },
                    body: bytes,
                }
            }
        }
    }

    pub fn handle_upload(&self, req: &HttpRequest) -> HttpResponse {
        let guard = self.inner.read();
        let inner = match guard.as_ref() {
            Some(i) => i,
            None => return HttpResponse::internal_error("not running"),
        };

        if req.body.len() as u64 > inner.max_upload_bytes {
            return HttpResponse::bad_request(format!(
                "file exceeds limit of {} MB",
                inner.max_upload_bytes / (1024 * 1024)
            ));
        }

        let dir = query_param(&req.query, "dir").unwrap_or_default();
        let name = match query_param(&req.query, "name") {
            Some(n) => n,
            None => return HttpResponse::bad_request("name required"),
        };

        if name.contains('/') || name.contains('\\') || name == ".." {
            return HttpResponse::bad_request("invalid filename");
        }

        let target_dir = match resolve(&inner.root, &dir) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };

        if !target_dir.is_dir() {
            if let Err(e) = std::fs::create_dir_all(&target_dir) {
                return HttpResponse::internal_error(e.to_string());
            }
        }

        let dest = target_dir.join(&name);
        match std::fs::write(&dest, &req.body) {
            Ok(()) => HttpResponse::ok_json(json!({
                "ok": true,
                "path": if dir.is_empty() { name } else { format!("{dir}/{name}") },
                "size": req.body.len(),
            })),
            Err(e) => HttpResponse::internal_error(e.to_string()),
        }
    }

    pub fn handle_mkdir(&self, req: &HttpRequest) -> HttpResponse {
        let root = match self.root() {
            Some(r) => r,
            None => return HttpResponse::internal_error("not running"),
        };

        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return HttpResponse::bad_request("expected JSON {path}"),
        };

        let rel = match body["path"].as_str() {
            Some(p) => p.to_string(),
            None => return HttpResponse::bad_request("path required"),
        };

        let abs = match resolve(&root, &rel) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };

        match std::fs::create_dir_all(&abs) {
            Ok(()) => HttpResponse::ok_json(json!({"ok": true, "path": rel})),
            Err(e) => HttpResponse::internal_error(e.to_string()),
        }
    }

    pub fn handle_delete(&self, req: &HttpRequest) -> HttpResponse {
        let guard = self.inner.read();
        let inner = match guard.as_ref() {
            Some(i) => i,
            None => return HttpResponse::internal_error("not running"),
        };

        if !inner.allow_delete {
            return HttpResponse::unauthorized();
        }

        let rel = match query_param(&req.query, "path") {
            Some(p) => p,
            None => return HttpResponse::bad_request("path required"),
        };

        let abs = match resolve(&inner.root, &rel) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };

        match crate::fs_util::move_to_bh_trash(&inner.root, &abs) {
            Ok(()) => HttpResponse::ok_json(json!({"ok": true})),
            Err(e) => HttpResponse::internal_error(e),
        }
    }

    pub fn handle_rename(&self, req: &HttpRequest) -> HttpResponse {
        let root = match self.root() {
            Some(r) => r,
            None => return HttpResponse::internal_error("not running"),
        };

        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return HttpResponse::bad_request("expected JSON {from, to}"),
        };

        let from = match body["from"].as_str() {
            Some(p) => p.to_string(),
            None => return HttpResponse::bad_request("from required"),
        };
        let to = match body["to"].as_str() {
            Some(p) => p.to_string(),
            None => return HttpResponse::bad_request("to required"),
        };

        let abs_from = match resolve(&root, &from) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };
        let abs_to = match resolve(&root, &to) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };

        match std::fs::rename(&abs_from, &abs_to) {
            Ok(()) => HttpResponse::ok_json(json!({"ok": true})),
            Err(e) => HttpResponse::internal_error(e.to_string()),
        }
    }

    pub fn handle_create_share(&self, req: &HttpRequest, created_by: &str) -> HttpResponse {
        let root = match self.root() {
            Some(r) => r,
            None => return HttpResponse::internal_error("not running"),
        };

        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return HttpResponse::bad_request("expected JSON"),
        };

        let rel = match body["path"].as_str() {
            Some(p) => p.to_string(),
            None => return HttpResponse::bad_request("path required"),
        };

        let abs = match resolve(&root, &rel) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };

        if !abs.exists() {
            return HttpResponse::not_found(format!("{rel} not found"));
        }

        let password = body["password"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let expires_hours = body["expires_hours"].as_u64();

        let token = Uuid::new_v4().to_string().replace('-', "");
        let name = abs
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| rel.clone());

        let share = Share {
            token: token.clone(),
            path: rel,
            is_dir: abs.is_dir(),
            name,
            password_protected: password.is_some(),
            password_hash: password.as_deref().map(crate::crypto::hash_password),
            expires_ms: expires_hours.map(|h| now_ms() + h * 3600 * 1000),
            created_by: created_by.to_string(),
            created_ms: now_ms(),
            download_count: 0,
        };

        let url = format!("/s/{}", token);
        let resp = json!({"ok": true, "token": token, "url": url, "share": share});

        self.shares.write().insert(token, share);
        HttpResponse::ok_json(resp)
    }

    pub fn handle_list_shares(&self, _role: &str) -> HttpResponse {
        let guard = self.shares.read();
        let shares: Vec<&Share> = guard.values().filter(|s| !s.is_expired()).collect();
        HttpResponse::ok_json(json!({"shares": shares}))
    }

    pub fn handle_delete_share(&self, req: &HttpRequest, user: &str, role: &str) -> HttpResponse {
        let token = match query_param(&req.query, "token") {
            Some(t) => t,
            None => return HttpResponse::bad_request("token required"),
        };

        let mut guard = self.shares.write();
        match guard.get(&token) {
            None => return HttpResponse::not_found("share not found"),
            Some(s) => {
                if role != "admin" && s.created_by != user {
                    return HttpResponse::unauthorized();
                }
            }
        }
        guard.remove(&token);
        HttpResponse::ok_json(json!({"ok": true}))
    }

    pub fn handle_share_access(&self, req: &HttpRequest, token: &str) -> HttpResponse {
        let root = match self.inner.read().as_ref().map(|i| i.root.clone()) {
            Some(r) => r,
            None => return HttpResponse::internal_error("not running"),
        };

        let share = match self
            .shares
            .read()
            .get(token)
            .filter(|s| !s.is_expired())
            .cloned()
        {
            Some(s) => s,
            None => return share_error_page("This share link is not valid or has expired."),
        };

        if share.password_protected {
            if req.method == "POST" {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
                let provided = body["password"].as_str().unwrap_or("");
                if !share.check_password(provided) {
                    return share_password_page(token, true);
                }
            } else {
                return share_password_page(token, false);
            }
        }

        let abs = match resolve(&root, &share.path) {
            Ok(p) => p,
            Err(e) => return share_error_page(&e),
        };

        if !abs.exists() {
            return share_error_page("The shared file no longer exists.");
        }

        if let Some(s) = self.shares.write().get_mut(token) {
            s.download_count += 1;
        }

        if abs.is_dir() {
            zip_directory(&abs, &share.path)
        } else {
            match std::fs::read(&abs) {
                Err(e) => HttpResponse::internal_error(e.to_string()),
                Ok(bytes) => {
                    let ct = mime_for_file(&share.path);
                    let name = abs
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| share.name.clone());
                    HttpResponse {
                        status: 200,
                        content_type: ct.to_string(),
                        headers: {
                            let mut h = HashMap::new();
                            h.insert(
                                "content-disposition".into(),
                                format!("attachment; filename=\"{}\"", name),
                            );
                            h
                        },
                        body: bytes,
                    }
                }
            }
        }
    }

    pub fn handle_preview(&self, req: &HttpRequest) -> HttpResponse {
        let root = match self.root() {
            Some(r) => r,
            None => return HttpResponse::internal_error("not running"),
        };

        let rel = match query_param(&req.query, "path") {
            Some(p) => p,
            None => return HttpResponse::bad_request("path required"),
        };

        let abs = match resolve(&root, &rel) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };

        if abs.is_dir() {
            return HttpResponse::bad_request("cannot preview a directory");
        }

        match std::fs::read(&abs) {
            Err(e) => HttpResponse::internal_error(e.to_string()),
            Ok(bytes) => {
                let ct = mime_for_file(&rel);
                let name = abs
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".to_string());
                HttpResponse {
                    status: 200,
                    content_type: ct.to_string(),
                    headers: {
                        let mut h = HashMap::new();
                        h.insert(
                            "content-disposition".into(),
                            format!("inline; filename=\"{}\"", name),
                        );
                        h.insert("cache-control".into(), "private, max-age=60".into());
                        h
                    },
                    body: bytes,
                }
            }
        }
    }

    pub fn handle_read(&self, req: &HttpRequest) -> HttpResponse {
        let root = match self.root() {
            Some(r) => r,
            None => return HttpResponse::internal_error("not running"),
        };

        let rel = match query_param(&req.query, "path") {
            Some(p) => p,
            None => return HttpResponse::bad_request("path required"),
        };

        let force = query_param(&req.query, "force")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);

        let abs = match resolve(&root, &rel) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };

        if abs.is_dir() {
            return HttpResponse::bad_request("cannot read a directory");
        }

        let meta = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(e) => return HttpResponse::internal_error(e.to_string()),
        };

        const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
        if meta.len() > MAX_READ_BYTES {
            return HttpResponse::bad_request(format!(
                "file too large to edit ({} MB > 2 MB limit)",
                meta.len() / (1024 * 1024)
            ));
        }

        if !force && !is_text_file(&rel) && !sniff_is_text(&abs) {
            return HttpResponse::bad_request(
                "file does not appear to be text. Use force=1 to open anyway.",
            );
        }

        match std::fs::read(&abs) {
            Err(e) => HttpResponse::internal_error(e.to_string()),
            Ok(bytes) => {
                let content = String::from_utf8(bytes)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                let lang = monaco_language(&rel);
                HttpResponse::ok_json(json!({
                    "content":  content,
                    "size":     meta.len(),
                    "language": lang,
                    "path":     rel,
                    "forced":   force,
                }))
            }
        }
    }

    pub fn handle_write(&self, req: &HttpRequest) -> HttpResponse {
        let guard = self.inner.read();
        let inner = match guard.as_ref() {
            Some(i) => i,
            None => return HttpResponse::internal_error("not running"),
        };

        if req.body.len() as u64 > inner.max_upload_bytes {
            return HttpResponse::bad_request(format!(
                "content exceeds limit of {} MB",
                inner.max_upload_bytes / (1024 * 1024)
            ));
        }

        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return HttpResponse::bad_request("expected JSON {path, content}"),
        };

        let rel = match body["path"].as_str() {
            Some(p) => p.to_string(),
            None => return HttpResponse::bad_request("path required"),
        };
        let content = match body["content"].as_str() {
            Some(c) => c.to_string(),
            None => return HttpResponse::bad_request("content required"),
        };

        let force = body["force"].as_bool().unwrap_or(false);

        let abs = match resolve(&inner.root, &rel) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };

        if !force && !is_text_file(&rel) {
            let ok_by_content = abs.exists() && sniff_is_text(&abs);
            if !ok_by_content {
                return HttpResponse::bad_request(
                    "file type not writable via editor. Pass \"force\": true to override.",
                );
            }
        }

        match std::fs::write(&abs, content.as_bytes()) {
            Ok(()) => HttpResponse::ok_json(json!({
                "ok":   true,
                "path": rel,
                "size": content.len(),
            })),
            Err(e) => HttpResponse::internal_error(e.to_string()),
        }
    }

    pub fn handle_copy(&self, req: &HttpRequest) -> HttpResponse {
        let root = match self.root() {
            Some(r) => r,
            None => return HttpResponse::internal_error("not running"),
        };

        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return HttpResponse::bad_request("expected JSON {from, to}"),
        };

        let from = match body["from"].as_str() {
            Some(p) => p.to_string(),
            None => return HttpResponse::bad_request("from required"),
        };
        let to = match body["to"].as_str() {
            Some(p) => p.to_string(),
            None => return HttpResponse::bad_request("to required"),
        };

        let abs_from = match resolve(&root, &from) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };
        let abs_to = match resolve(&root, &to) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };

        if !abs_from.exists() {
            return HttpResponse::not_found(format!("{from} not found"));
        }
        if abs_to.exists() {
            return HttpResponse::bad_request(format!("destination {to} already exists"));
        }

        let result = if abs_from.is_dir() {
            copy_dir_all(&abs_from, &abs_to)
        } else {
            std::fs::copy(&abs_from, &abs_to)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };

        match result {
            Ok(()) => HttpResponse::ok_json(json!({"ok": true, "from": from, "to": to})),
            Err(e) => HttpResponse::internal_error(e),
        }
    }

    pub fn handle_search(&self, req: &HttpRequest) -> HttpResponse {
        let root = match self.root() {
            Some(r) => r,
            None => return HttpResponse::internal_error("not running"),
        };

        let rel = query_param(&req.query, "path").unwrap_or_default();
        let query = match query_param(&req.query, "q") {
            Some(q) if !q.is_empty() => q.to_lowercase(),
            _ => return HttpResponse::bad_request("q required"),
        };
        let max_results: usize = query_param(&req.query, "max_results")
            .and_then(|s| s.parse().ok())
            .unwrap_or(200)
            .min(500);

        let base = match resolve(&root, &rel) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };

        if !base.is_dir() {
            return HttpResponse::bad_request("path must be a directory");
        }

        let mut results: Vec<Value> = Vec::new();
        search_dir(&base, &base, &query, max_results, &mut results);

        HttpResponse::ok_json(json!({
            "path":      rel,
            "query":     query,
            "results":   results,
            "truncated": results.len() >= max_results,
        }))
    }

    pub fn handle_thumb(&self, req: &HttpRequest) -> HttpResponse {
        self.handle_preview(req)
    }

    pub fn handle_detect(&self, req: &HttpRequest) -> HttpResponse {
        let root = match self.root() {
            Some(r) => r,
            None => return HttpResponse::internal_error("not running"),
        };

        let rel = match query_param(&req.query, "path") {
            Some(p) => p,
            None => return HttpResponse::bad_request("path required"),
        };

        let abs = match resolve(&root, &rel) {
            Ok(p) => p,
            Err(e) => return HttpResponse::bad_request(e),
        };

        if !abs.exists() {
            return HttpResponse::not_found(format!("{rel} not found"));
        }

        let is_dir = abs.is_dir();
        let size = abs.metadata().map(|m| m.len()).unwrap_or(0);
        let by_extension = is_text_file(&rel);
        let by_content = if is_dir { false } else { sniff_is_text(&abs) };
        let is_text = by_extension || by_content;
        let language = monaco_language(&rel);

        HttpResponse::ok_json(json!({
            "is_text":      is_text,
            "by_extension": by_extension,
            "by_content":   by_content,
            "language":     language,
            "size":         size,
            "is_dir":       is_dir,
        }))
    }
}
