//! Anonymous HTTP CAS transport; runtime range reads/cache remain upstream-owned.
use anyhow::{ensure, Context, Result};
use overlaybd::{
    config::{CacheConfig, GlobalConfig, ImageConfig, LayerConfig},
    image_service::ImageService,
    virtual_file::VirtualFile,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteLayer {
    pub repo_blob_url: String,
    pub digest: String,
    pub size: u64,
}
pub fn url(value: &str) -> Result<reqwest::Url> {
    let u = reqwest::Url::parse(value)?;
    ensure!(
        matches!(u.scheme(), "http" | "https")
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none(),
        "expected anonymous HTTP(S) URL without query/fragment"
    );
    Ok(u)
}
fn digest_valid(d: &str) -> bool {
    d.strip_prefix("sha256:").is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}
impl RemoteLayer {
    pub fn validate(&self) -> Result<()> {
        url(&self.repo_blob_url)?;
        ensure!(
            digest_valid(&self.digest) && self.size > 0,
            "invalid remote digest/size"
        );
        Ok(())
    }
    pub fn url(&self) -> String {
        format!(
            "{}/{}",
            self.repo_blob_url.trim_end_matches('/'),
            self.digest
        )
    }
    pub fn config(&self) -> LayerConfig {
        LayerConfig {
            repo_blob_url: self.repo_blob_url.clone(),
            digest: self.digest.clone(),
            size: self.size,
            ..Default::default()
        }
    }
    pub fn from_config(c: &LayerConfig) -> Result<Self> {
        let r = Self {
            repo_blob_url: c.repo_blob_url.clone(),
            digest: c.digest.clone(),
            size: c.size,
        };
        r.validate()?;
        Ok(r)
    }
}
pub async fn service(cache: &Path, remote: bool) -> Result<ImageService> {
    ImageService::new(GlobalConfig {
        enable_audit: false,
        credential_file_path: String::new(),
        remote_io_workers: 0,
        cache_config: CacheConfig {
            cache_dir: cache.to_string_lossy().into_owned(),
            cache_size_gb: if remote { 1 } else { 0 },
            refill_size: 65536,
            ..Default::default()
        },
        ..Default::default()
    })
    .await
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}
fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
async fn file_digest(path: &Path) -> Result<(String, u64)> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut h = Sha256::new();
    let mut size = 0;
    let mut b = vec![0; 1024 * 1024];
    loop {
        let n = f.read(&mut b).await?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
        size += n as u64;
    }
    Ok((format!("sha256:{:x}", h.finalize()), size))
}
async fn verify_response(mut r: reqwest::Response, digest: &str, size: u64) -> Result<()> {
    ensure!(
        r.status() == reqwest::StatusCode::OK,
        "object GET failed: {}",
        r.status()
    );
    let mut h = Sha256::new();
    let mut n = 0;
    while let Some(b) = r.chunk().await? {
        n += b.len() as u64;
        ensure!(n <= size, "object exceeds expected size");
        h.update(&b);
    }
    ensure!(
        n == size && format!("sha256:{:x}", h.finalize()) == digest,
        "object digest/size mismatch"
    );
    Ok(())
}
async fn put(path: &Path, endpoint: &str, digest: &str, size: u64) -> Result<()> {
    let c = client()?;
    let r = c
        .put(url(endpoint)?)
        .header("If-None-Match", "*")
        .header("Content-Length", size)
        .body(reqwest::Body::from(tokio::fs::File::open(path).await?))
        .send()
        .await?;
    ensure!(
        r.status().is_success() || r.status() == reqwest::StatusCode::PRECONDITION_FAILED,
        "CAS PUT failed: {}",
        r.status()
    );
    // Verify an existing object too; a conditional conflict alone proves nothing.
    verify_response(c.get(endpoint).send().await?, digest, size).await
}
pub async fn publish(config: &Path, repository: &str) -> Result<Value> {
    url(repository)?;
    let cfg: ImageConfig = serde_json::from_slice(&tokio::fs::read(config).await?)?;
    let mut lowers = vec![];
    for l in cfg.lowers {
        ensure!(
            !l.file.is_empty(),
            "publish expects an immutable local export"
        );
        let p = config.parent().context("config parent")?.join(l.file);
        let (digest, size) = file_digest(&p).await?;
        let r = RemoteLayer {
            repo_blob_url: format!("{}/blobs", repository.trim_end_matches('/')),
            digest,
            size,
        };
        put(&p, &r.url(), &r.digest, r.size).await?;
        lowers.push(r);
    }
    let bytes = serde_json::to_vec(&json!({"lowers":lowers}))?;
    let digest = hash(&bytes);
    let endpoint = format!("{}/manifests/{digest}", repository.trim_end_matches('/'));
    let p = config.with_file_name("remote-manifest.json");
    tokio::fs::write(&p, &bytes).await?;
    put(&p, &endpoint, &digest, bytes.len() as u64).await?;
    Ok(json!({"manifest":endpoint,"digest":digest}))
}
pub async fn manifest(endpoint: &str) -> Result<Vec<RemoteLayer>> {
    let u = url(endpoint)?;
    let expected = u
        .path_segments()
        .and_then(|mut s| s.next_back())
        .context("manifest digest")?;
    ensure!(
        digest_valid(expected),
        "manifest URL must end in sha256:<digest>"
    );
    let mut r = client()?.get(u.clone()).send().await?.error_for_status()?;
    let mut bytes = vec![];
    while let Some(b) = r.chunk().await? {
        ensure!(
            bytes.len() + b.len() <= 1024 * 1024,
            "manifest exceeds 1 MiB"
        );
        bytes.extend_from_slice(&b);
    }
    ensure!(hash(&bytes) == expected, "manifest digest mismatch");
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Manifest {
        lowers: Vec<RemoteLayer>,
    }
    let m: Manifest = serde_json::from_slice(&bytes)?;
    ensure!(
        !m.lowers.is_empty() && m.lowers.len() < 253,
        "expected 1..252 remote layers"
    );
    for l in &m.lowers {
        l.validate()?;
    }
    Ok(m.lowers)
}
pub async fn layer_size(layer: &RemoteLayer, cache: &Path) -> Result<u64> {
    layer.validate()?;
    let svc = service(cache, true).await?;
    let f = svc
        .open_remote_blob_with_size(&layer.url(), Some(layer.size))
        .await?;
    let f = if overlaybd::zfile::is_zfile(f.clone()).await? == 1 {
        overlaybd::zfile::zfile_open_ro_vfile(f, true).await?
    } else {
        f
    };
    overlaybd::index_file::LSMTReadOnlyFile::open(f)
        .await?
        .size()
        .await
}
pub async fn fetch(layer: &RemoteLayer, dest: &Path) -> Result<()> {
    layer.validate()?;
    let temp = dest.with_file_name(uuid::Uuid::new_v4().to_string());
    let result = async {
        let mut f = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await?;
        let mut r = client()?.get(layer.url()).send().await?;
        ensure!(
            r.status() == reqwest::StatusCode::OK,
            "object GET failed: {}",
            r.status()
        );
        let mut h = Sha256::new();
        let mut n = 0;
        while let Some(b) = r.chunk().await? {
            n += b.len() as u64;
            ensure!(n <= layer.size, "object too large");
            h.update(&b);
            f.write_all(&b).await?;
        }
        ensure!(
            n == layer.size && format!("sha256:{:x}", h.finalize()) == layer.digest,
            "object digest/size mismatch"
        );
        f.sync_all().await?;
        drop(f);
        crate::durable_file(&temp)?;
        ensure!(!dest.exists(), "destination already exists");
        tokio::fs::rename(&temp, dest).await?;
        crate::sync_directory(dest.parent().context("parent")?)?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_identity_and_url_validation() {
        for value in [
            "file:///tmp/layer",
            "ftp://host/a",
            "https://u:p@host/a",
            "https://host/a?token=secret",
            "https://host/a#fragment",
        ] {
            assert!(url(value).is_err(), "{value}");
        }
        for value in [
            "http://127.0.0.1:8123/blobs",
            "https://example.com/repo/blobs",
        ] {
            assert!(url(value).is_ok());
        }
        let mut r = RemoteLayer {
            repo_blob_url: "https://example.com/blobs".into(),
            digest: format!("sha256:{}", "a".repeat(64)),
            size: 4096,
        };
        assert!(r.validate().is_ok());
        assert_eq!(
            RemoteLayer::from_config(&r.config()).unwrap().digest,
            r.digest
        );
        r.digest = "sha256:../../oops".into();
        assert!(r.validate().is_err());
        r.digest = format!("sha256:{}", "a".repeat(64));
        r.size = 0;
        assert!(r.validate().is_err());
        assert!(serde_json::from_value::<RemoteLayer>(json!({"repoBlobUrl":"http://host/blobs", "digest":r.digest,"size":4096,"file":"/tmp/escape"})).is_err());
    }
}
