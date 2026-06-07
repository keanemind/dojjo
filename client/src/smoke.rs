//! Dev smoke: create dojo → TUS upload → manifest → GET each path in `apply_order`.

use anyhow::Context;
use reqwest::header::{ETAG, IF_NONE_MATCH};
use sha2::{Digest, Sha256};

use crate::manifest::fetch_manifest;
use crate::tus::{post_create_dojo, tus_upload_bytes};

pub async fn run(client: &reqwest::Client) -> anyhow::Result<()> {
    let api_base = crate::config::resolve_api_base(None)?;
    assert!(!api_base.is_empty(), "api base must not be empty");

    let created = post_create_dojo(&client, &api_base).await?;
    let id = created.id.clone();
    assert!(!id.is_empty(), "dojo id must not be empty");

    println!("Dojo created with id: {id}");

    let file_bytes = b"dojjo-smoke-payload".to_vec();
    anyhow::ensure!(!file_bytes.is_empty(), "smoke payload must be non-empty");
    let len = file_bytes.len() as u64;
    let expected_sha = hex::encode(Sha256::digest(&file_bytes));

    let jj_rel_path = "smoke/hello.txt";
    tus_upload_bytes(&client, &api_base, &id, jj_rel_path, &file_bytes)
        .await
        .context("tus upload smoke file")?;

    println!("Uploaded via TUS to dojo {id} at mirror/{jj_rel_path}");

    let body = fetch_manifest(&client, &api_base, &id).await?;
    anyhow::ensure!(!body.revision.is_empty(), "revision must be non-empty");
    let row = body
        .entries
        .get(jj_rel_path)
        .context("manifest must list uploaded path")?;
    anyhow::ensure!(row.size == len, "manifest size mismatch");
    anyhow::ensure!(row.sha256 == expected_sha, "manifest sha256 mismatch");

    anyhow::ensure!(
        body.apply_order.iter().any(|p| p == jj_rel_path),
        "apply_order must include uploaded path"
    );
    anyhow::ensure!(
        body.apply_order.len() == body.entries.len(),
        "apply_order must be a permutation of manifest entries"
    );

    let base = api_base.trim_end_matches('/');
    for p in &body.apply_order {
        let url = format!("{base}/dojo/{id}/mirror/{p}");
        let r = client.get(&url).send().await.with_context(|| format!("GET {p}"))?;
        let r = r
            .error_for_status()
            .with_context(|| format!("GET {p} status"))?;
        let bytes = r.bytes().await.with_context(|| format!("GET {p} body"))?;
        if p == jj_rel_path {
            anyhow::ensure!(bytes.as_ref() == file_bytes.as_slice());
        }
    }
    println!("Fetched every mirror object in manifest apply_order (pull order)");

    let manifest_url = format!("{base}/dojo/{id}/mirror/manifest");
    let man_res = client.get(&manifest_url).send().await.context("manifest etag")?;
    let man_res = man_res.error_for_status().context("manifest status")?;
    let etag = man_res
        .headers()
        .get(ETAG)
        .and_then(|h| h.to_str().ok())
        .context("manifest must include ETag")?
        .to_string();
    let _body2: serde_json::Value = man_res.json().await.context("manifest json")?;

    let r304 = client
        .get(&manifest_url)
        .header(IF_NONE_MATCH, etag)
        .send()
        .await
        .context("manifest conditional")?;
    anyhow::ensure!(
        r304.status().as_u16() == 304,
        "expected 304, got {}",
        r304.status()
    );
    println!("Verified manifest ETag / If-None-Match (304)");

    Ok(())
}
