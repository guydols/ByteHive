use bytehive_core::HttpResponse;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim_start_matches('/').replace('\\', "/");

    for component in Path::new(&rel).components() {
        match component {
            Component::ParentDir => return Err("path traversal not allowed".into()),
            Component::Prefix(_) | Component::RootDir => {
                return Err("absolute paths not allowed".into())
            }
            _ => {}
        }
    }

    let abs = root.join(&rel);

    let root_canon = root.canonicalize().map_err(|e| e.to_string())?;
    if abs.exists() {
        let abs_canon = abs.canonicalize().map_err(|e| e.to_string())?;
        if !abs_canon.starts_with(&root_canon) {
            return Err("path escapes root".into());
        }
    } else {
        if let Some(parent) = abs.parent() {
            if parent.exists() {
                let parent_canon = parent.canonicalize().map_err(|e| e.to_string())?;
                if !parent_canon.starts_with(&root_canon) {
                    return Err("path escapes root".into());
                }
            }
        }
    }

    Ok(abs)
}

pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub fn search_dir(base: &Path, cur: &Path, query: &str, max: usize, results: &mut Vec<Value>) {
    if results.len() >= max {
        return;
    }
    let Ok(rd) = std::fs::read_dir(cur) else {
        return;
    };
    for entry in rd.flatten() {
        if results.len() >= max {
            return;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // Never expose the internal .bh_filesync folder
        if name == ".bh_filesync" {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);

        if name.to_lowercase().contains(query) {
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            results.push(json!({
                "name":   name,
                "path":   rel,
                "is_dir": is_dir,
                "size":   size,
                "mtime":  mtime,
                "ext":    extension(&name),
            }));
        }

        if is_dir && path.is_dir() {
            search_dir(base, &path, query, max, results);
        }
    }
}

pub fn zip_directory(abs: &Path, _rel: &str) -> HttpResponse {
    use std::io::Cursor;

    let mut buf = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        fn add_dir(
            zip: &mut zip::ZipWriter<&mut Cursor<Vec<u8>>>,
            base: &Path,
            cur: &Path,
            opts: zip::write::SimpleFileOptions,
        ) {
            if let Ok(rd) = std::fs::read_dir(cur) {
                for entry in rd.flatten() {
                    let path = entry.path();
                    let rel = path.strip_prefix(base).unwrap_or(&path);
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    if path.is_dir() {
                        let _ = zip.add_directory(format!("{rel_str}/"), opts);
                        add_dir(zip, base, &path, opts);
                    } else if let Ok(bytes) = std::fs::read(&path) {
                        if zip.start_file(rel_str, opts).is_ok() {
                            let _ = zip.write_all(&bytes);
                        }
                    }
                }
            }
        }

        add_dir(&mut zip, abs, abs, opts);
        let _ = zip.finish();
    }

    let name = abs
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());

    HttpResponse {
        status: 200,
        content_type: "application/zip".into(),
        headers: {
            let mut h = HashMap::new();
            h.insert(
                "content-disposition".into(),
                format!("attachment; filename=\"{name}.zip\""),
            );
            h
        },
        body: buf.into_inner(),
    }
}

/// Move `abs` into `.bh_filesync/trash` inside `root`.
/// The item is stored as `<unix_ms>_<original_name>` in the trash folder.
/// Falls back to copy+delete if a cross-device rename fails.
pub fn move_to_bh_trash(root: &Path, abs: &Path) -> Result<(), String> {
    let trash_dir = root.join(".bh_filesync").join("trash");
    std::fs::create_dir_all(&trash_dir).map_err(|e| e.to_string())?;

    let file_name = abs
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
        .to_string_lossy();
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let trash_dest = trash_dir.join(format!("{unix_ms}_{file_name}"));

    if std::fs::rename(abs, &trash_dest).is_err() {
        // Cross-device or other issue — fall back to copy then delete
        if abs.is_dir() {
            copy_dir_all(abs, &trash_dest)?;
            std::fs::remove_dir_all(abs).map_err(|e| e.to_string())?;
        } else {
            std::fs::copy(abs, &trash_dest).map_err(|e| e.to_string())?;
            std::fs::remove_file(abs).map_err(|e| e.to_string())?;
        }
    }

    Ok(())
}

pub fn extension(name: &str) -> &str {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

pub fn mime_for_file(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "tiff" | "tif" => "image/tiff",
        "avif" => "image/avif",

        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "ogv" => "video/ogg",

        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "m4a" => "audio/mp4",
        "opus" => "audio/opus",

        "txt" | "md" | "rst" | "ini" | "cfg" | "conf" | "env" | "gitignore" | "log" | "csv" => {
            "text/plain; charset=utf-8"
        }
        "html" | "htm" => "text/html; charset=utf-8",
        "css" | "scss" | "sass" => "text/css; charset=utf-8",
        "js" | "mjs" | "cjs" | "jsx" => "application/javascript; charset=utf-8",
        "ts" | "tsx" | "mts" => "application/typescript; charset=utf-8",
        "json" | "jsonc" => "application/json; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "yml" | "yaml" => "text/yaml; charset=utf-8",
        "toml" => "application/toml; charset=utf-8",
        "rs" | "py" | "go" | "java" | "c" | "cpp" | "h" | "hpp" | "cs" | "rb" | "php" | "sh"
        | "bash" | "zsh" | "sql" | "kt" | "swift" | "lua" | "r" | "pl" | "ex" | "exs" | "hs"
        | "scala" | "proto" | "graphql" | "gql" | "tf" | "hcl" | "nix" | "vue" | "svelte" => {
            "text/plain; charset=utf-8"
        }

        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" | "tgz" => "application/gzip",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/x-rar-compressed",

        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}
