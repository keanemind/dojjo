//! Smart HTTP Git via `git http-backend` (CGI). Routes `/git/{dojo_id}.git/*`.
//!
//! Requires `git` on `PATH`. No auth — tailnet trust only ([`GIT_HTTP_EXPORT_ALL`]).

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::git_remote::bare_repo_path;
use crate::tus_handlers::assert_safe_dojo_external_id;
use crate::{AppError, AppState};

const GIT_HTTP_EXPORT_ALL: &str = "1";
/// `git http-backend` enables receive-pack only for "authenticated" CGI clients.
const GIT_HTTP_REMOTE_USER: &str = "dojjo";

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/git/{*rest}", any(handle_git_http))
}

async fn handle_git_http(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, AppError> {
    let (parts, body) = request.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let headers = parts.headers.clone();

    let path = uri.path();
    assert!(path.starts_with("/git/"), "git router must only see /git/* paths");
    let rest = path.strip_prefix("/git/").expect("/git/ prefix");
    assert!(!rest.is_empty(), "git path after /git/ must not be empty");

    let (dojo_id, path_info) = parse_git_http_rest(rest)?;
    assert_safe_dojo_external_id(dojo_id)?;

    let bare = bare_repo_path(&state.data_dir, dojo_id)?;
    if !bare.is_dir() {
        return Err(AppError::NotFound);
    }
    assert!(bare.is_dir(), "bare repo must exist before git HTTP");

    let _dojo_io = state.dojo_io_locks.lock(dojo_id).await;

    let query = uri.query().unwrap_or("");
    let body_bytes = axum::body::to_bytes(body, 512 * 1024 * 1024)
        .await
        .map_err(|e| AppError::Internal(anyhow::Error::from(e)))?;

    let path_info = git_path_info_for_backend(dojo_id, &path_info)?;
    let cgi_out = run_git_http_backend(
        &method,
        &path_info,
        query,
        &headers,
        &state.data_dir,
        &body_bytes,
    )
    .await?;

    Ok(cgi_response_to_axum(&cgi_out))
}

/// Split `{dojo_id}.git/{tail}` from the path segment after `/git/`.
fn parse_git_http_rest(rest: &str) -> Result<(&str, String), AppError> {
    assert!(!rest.is_empty(), "rest must not be empty");
    const MARKER: &str = ".git/";
    if let Some((dojo_id, tail)) = rest.split_once(MARKER) {
        assert!(!dojo_id.is_empty(), "dojo id must not be empty");
        assert!(!tail.is_empty(), "git path after .git/ must not be empty");
        if tail.contains("..") {
            return Err(AppError::BadRequest);
        }
        let path_info = if tail.starts_with('/') {
            tail.to_string()
        } else {
            format!("/{tail}")
        };
        assert!(
            path_info.starts_with('/'),
            "PATH_INFO must start with /"
        );
        return Ok((dojo_id, path_info));
    }
    if let Some(dojo_id) = rest.strip_suffix(".git") {
        assert!(!dojo_id.is_empty(), "dojo id must not be empty");
        return Ok((dojo_id, "/".to_string()));
    }
    Err(AppError::NotFound)
}

/// `PATH_INFO` for `git http-backend` with [`GIT_PROJECT_ROOT`] = `data_dir`.
fn git_path_info_for_backend(dojo_id: &str, git_tail: &str) -> Result<String, AppError> {
    assert_safe_dojo_external_id(dojo_id)?;
    assert!(
        git_tail.starts_with('/'),
        "git tail after .git must start with /"
    );
    let path_info = format!("/{dojo_id}/bare.git{git_tail}");
    assert!(
        path_info.contains("/bare.git/") || path_info.ends_with("/bare.git/"),
        "PATH_INFO must locate bare.git under dojo id"
    );
    if path_info.contains("..") {
        return Err(AppError::BadRequest);
    }
    Ok(path_info)
}

async fn run_git_http_backend(
    method: &Method,
    path_info: &str,
    query_string: &str,
    headers: &HeaderMap,
    project_root: &Path,
    body: &[u8],
) -> Result<Vec<u8>, AppError> {
    assert!(project_root.is_absolute(), "GIT_PROJECT_ROOT must be absolute");
    assert!(
        path_info.starts_with('/'),
        "PATH_INFO must start with /"
    );
    let project_root = project_root
        .to_str()
        .ok_or(AppError::BadRequest)?;

    let mut cmd = Command::new("git");
    cmd.arg("http-backend")
        .env("GIT_HTTP_EXPORT_ALL", GIT_HTTP_EXPORT_ALL)
        .env("GIT_PROJECT_ROOT", project_root)
        .env("REMOTE_USER", GIT_HTTP_REMOTE_USER)
        .env("PATH_INFO", path_info)
        .env("REQUEST_METHOD", method.as_str())
        .env("QUERY_STRING", query_string)
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("SERVER_SOFTWARE", "dojjo-sync-server")
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if !body.is_empty() {
        cmd.env("CONTENT_LENGTH", body.len().to_string());
    }
    if let Some(ct) = headers.get(axum::http::header::CONTENT_TYPE) {
        if let Ok(s) = ct.to_str() {
            cmd.env("CONTENT_TYPE", s);
        }
    }

    apply_http_header_env(&mut cmd, headers);

    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::Internal(anyhow::Error::from(e)))?;
    if !body.is_empty() {
        let mut stdin = child
            .stdin
            .take()
            .expect("stdin piped when body non-empty");
        stdin
            .write_all(body)
            .await
            .map_err(|e| AppError::Internal(anyhow::Error::from(e)))?;
        stdin
            .shutdown()
            .await
            .map_err(|e| AppError::Internal(anyhow::Error::from(e)))?;
    } else if let Some(mut stdin) = child.stdin.take() {
        stdin
            .shutdown()
            .await
            .map_err(|e| AppError::Internal(anyhow::Error::from(e)))?;
    }

    let out = child
        .wait_with_output()
        .await
        .map_err(|e| AppError::Internal(anyhow::Error::from(e)))?;
    if !out.stderr.is_empty() {
        tracing::debug!(
            stderr = %String::from_utf8_lossy(&out.stderr),
            "git http-backend stderr"
        );
    }
    if !out.status.success() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "git http-backend exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(out.stdout)
}

fn apply_http_header_env(cmd: &mut Command, headers: &HeaderMap) {
    if let Some(host) = headers.get(axum::http::header::HOST) {
        if let Ok(s) = host.to_str() {
            cmd.env("HTTP_HOST", s);
            if let Some((name, port)) = s.split_once(':') {
                cmd.env("SERVER_NAME", name);
                cmd.env("SERVER_PORT", port);
            } else {
                cmd.env("SERVER_NAME", s);
            }
        }
    }
    for (name, value) in headers.iter() {
        if name == axum::http::header::HOST {
            continue;
        }
        let Ok(v) = value.to_str() else {
            continue;
        };
        let key = name.as_str().replace('-', "_").to_ascii_uppercase();
        let env = format!("HTTP_{key}");
        cmd.env(env, v);
    }
}

/// Parsed CGI output from `git http-backend` (Status + headers + body).
struct CgiResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

fn parse_cgi_output(raw: &[u8]) -> Result<CgiResponse, AppError> {
    assert!(!raw.is_empty(), "CGI output must not be empty");
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)));
    let (header_end, sep_len) = sep.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("git http-backend: missing CGI header/body separator"))
    })?;
    let header_block = &raw[..header_end];
    let body = raw[header_end + sep_len..].to_vec();

    let header_text = std::str::from_utf8(header_block)
        .map_err(|_| AppError::Internal(anyhow::anyhow!("git http-backend: invalid UTF-8 headers")))?;

    let mut status = StatusCode::OK;
    let mut headers = HeaderMap::new();

    for line in header_text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("Status") {
            status = parse_status_line(value)?;
            continue;
        }
        if let Ok(hn) = HeaderName::try_from(name) {
            if let Ok(hv) = HeaderValue::from_str(value) {
                headers.insert(hn, hv);
            }
        }
    }

    assert!(
        status.as_u16() >= 100 && status.as_u16() < 600,
        "status must be valid HTTP"
    );
    Ok(CgiResponse {
        status,
        headers,
        body,
    })
}

fn parse_status_line(value: &str) -> Result<StatusCode, AppError> {
    let code_str = value.split_whitespace().next().unwrap_or("");
    let code: u16 = code_str
        .parse()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("invalid CGI Status: {value:?}")))?;
    StatusCode::from_u16(code)
        .map_err(|_| AppError::Internal(anyhow::anyhow!("invalid CGI status code: {code}")))
}

fn cgi_response_to_axum(raw: &[u8]) -> Response {
    let parsed = match parse_cgi_output(raw) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let mut builder = Response::builder().status(parsed.status);
    for (name, value) in parsed.headers.iter() {
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(parsed.body))
        .expect("valid CGI response body")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::tempdir;

    #[test]
    fn git_path_info_for_backend_maps_disk_layout() {
        let p = git_path_info_for_backend("my-dojo", "/info/refs").unwrap();
        assert_eq!(p, "/my-dojo/bare.git/info/refs");
    }

    #[test]
    fn parse_git_http_rest_splits_dojo_and_path() {
        let (id, path) = parse_git_http_rest("my-dojo.git/info/refs").unwrap();
        assert_eq!(id, "my-dojo");
        assert_eq!(path, "/info/refs");
    }

    #[test]
    fn parse_git_http_rest_rejects_dotdot() {
        assert!(parse_git_http_rest("x.git/../etc/passwd").is_err());
    }

    #[test]
    fn parse_cgi_status_and_body() {
        let raw = b"Status: 200 OK\r\nContent-Type: application/x-git-upload-pack-advertisement\r\n\r\nPKT";
        let r = parse_cgi_output(raw).unwrap();
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.body, b"PKT");
    }

    #[test]
    fn git_http_backend_info_refs() {
        if StdCommand::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = tempdir().unwrap();
        let data = dir.path().join("data");
        let bare = data.join("my-dojo").join("bare.git");
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        assert!(
            StdCommand::new("git")
                .args(["init", "--bare", bare.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        let data = data.canonicalize().unwrap();
        let path_info = git_path_info_for_backend("my-dojo", "/info/refs").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt
            .block_on(run_git_http_backend(
                &Method::GET,
                &path_info,
                "service=git-upload-pack",
                &HeaderMap::new(),
                &data,
                b"",
            ))
            .expect("git http-backend");
        assert!(out.windows(4).any(|w| w == b"\r\n\r\n") || out.windows(2).any(|w| w == b"\n\n"));
        let parsed = parse_cgi_output(&out).unwrap();
        assert_eq!(parsed.status, StatusCode::OK);
        let body = String::from_utf8_lossy(&parsed.body);
        assert!(
            body.contains("git-upload-pack"),
            "advertisement should name git-upload-pack service"
        );
        assert!(!parsed.body.is_empty(), "advertisement body must not be empty");
    }
}
