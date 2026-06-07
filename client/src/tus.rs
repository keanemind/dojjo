//! TUS create + PATCH upload and `POST /dojo` helper.

use std::collections::HashMap;

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::Client;
use reqwest::header::LOCATION;

use serde::Deserialize;

const TUS_VERSION: &str = "1.0.0";
const OFFSET_CONTENT_TYPE: &str = "application/offset+octet-stream";

#[derive(Debug, Clone, Deserialize, Eq, PartialEq)]
pub struct DojoPublic {
    pub id: String,
    pub git_remote_url: String,
}

fn manifest_base(api_base: &str) -> String {
    let b = api_base.trim_end_matches('/');
    assert!(!b.is_empty(), "api_base must not be empty");
    b.to_string()
}

fn site_origin(api_base: &str) -> anyhow::Result<String> {
    let b = manifest_base(api_base);
    let rest = b
        .strip_prefix("http://")
        .or_else(|| b.strip_prefix("https://"))
        .context("api_base must be http(s) URL")?;
    assert!(!rest.is_empty(), "api_base host part must not be empty");
    let scheme = if b.starts_with("https://") {
        "https://"
    } else {
        "http://"
    };
    let host_port = rest.split('/').next().context("api_base must have host")?;
    assert!(!host_port.is_empty(), "host must not be empty");
    Ok(format!("{scheme}{host_port}"))
}

pub async fn post_create_dojo(client: &Client, api_base: &str) -> anyhow::Result<DojoPublic> {
    assert!(!api_base.is_empty(), "api_base must not be empty");
    let url = format!("{}/dojo", manifest_base(api_base));
    let mut body = HashMap::new();
    body.insert("create_type", "Upload");
    let res = client.post(&url).json(&body).send().await.context("POST /dojo")?;
    let res = res.error_for_status().context("POST /dojo status")?;
    let v: serde_json::Value = res.json().await.context("POST /dojo json")?;
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .context("dojo response must include string id")?;
    assert!(!id.is_empty(), "dojo id must not be empty");
    let git_remote_url = v
        .get("git_remote_url")
        .and_then(|x| x.as_str())
        .context("dojo response must include git_remote_url (upgrade sync-server)")?;
    assert!(
        !git_remote_url.is_empty(),
        "git_remote_url must not be empty"
    );
    Ok(DojoPublic {
        id: id.to_string(),
        git_remote_url: git_remote_url.to_string(),
    })
}

pub async fn fetch_dojo_public(client: &Client, api_base: &str, dojo_id: &str) -> anyhow::Result<DojoPublic> {
    assert!(!dojo_id.is_empty(), "dojo_id must not be empty");
    let url = format!("{}/dojo/{}", manifest_base(api_base), dojo_id);
    let res = client.get(&url).send().await.context("GET /dojo/{id}")?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("dojo not found (check --dojo-id and --api-base)");
    }
    let res = res.error_for_status().context("GET /dojo/{id} status")?;
    let v: serde_json::Value = res.json().await.context("GET /dojo json")?;
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .context("dojo response must include string id")?;
    let git_remote_url = v
        .get("git_remote_url")
        .and_then(|x| x.as_str())
        .context("dojo response must include git_remote_url")?;
    assert!(!id.is_empty(), "dojo id must not be empty");
    assert!(!git_remote_url.is_empty(), "git_remote_url must not be empty");
    Ok(DojoPublic {
        id: id.to_string(),
        git_remote_url: git_remote_url.to_string(),
    })
}

/// Upload one file via TUS (single PATCH). `jj_rel_path` uses forward slashes.
pub async fn tus_upload_bytes(
    client: &Client,
    api_base: &str,
    dojo_id: &str,
    jj_rel_path: &str,
    bytes: &[u8],
) -> anyhow::Result<()> {
    assert!(!dojo_id.is_empty(), "dojo_id must not be empty");
    assert!(!jj_rel_path.is_empty(), "jj_rel_path must not be empty");
    assert!(
        bytes.len() <= 64 * 1024 * 1024,
        "upload chunk must respect mirror blob cap"
    );
    let len = bytes.len() as u64;

    let upload_metadata = format!("jj_rel_path {}", STANDARD.encode(jj_rel_path.as_bytes()));
    let create_url = format!("{}/dojo/{dojo_id}/uploads", manifest_base(api_base));
    let create_res = client
        .post(&create_url)
        .header("Tus-Resumable", TUS_VERSION)
        .header("Upload-Length", len.to_string())
        .header("Upload-Metadata", upload_metadata)
        .send()
        .await
        .with_context(|| format!("tus create {jj_rel_path}"))?;
    let create_res = create_res
        .error_for_status()
        .with_context(|| format!("tus create status {jj_rel_path}"))?;

    let location = create_res
        .headers()
        .get(LOCATION)
        .and_then(|v| v.to_str().ok())
        .context("TUS create must return Location")?;
    assert!(
        location.starts_with("/api/"),
        "Location must be relative API path"
    );
    let origin = site_origin(api_base)?;
    let upload_url = format!("{origin}{location}");
    assert!(
        upload_url.starts_with("http"),
        "upload URL must be absolute http(s)"
    );

    let patch_res = client
        .patch(&upload_url)
        .header("Tus-Resumable", TUS_VERSION)
        .header("Upload-Offset", "0")
        .header(reqwest::header::CONTENT_TYPE, OFFSET_CONTENT_TYPE)
        .body(bytes.to_vec())
        .send()
        .await
        .with_context(|| format!("tus patch {jj_rel_path}"))?;
    patch_res
        .error_for_status()
        .with_context(|| format!("tus patch status {jj_rel_path}"))?;
    Ok(())
}
