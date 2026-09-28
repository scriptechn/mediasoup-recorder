//! S3 upload of a finished spool (`docs/spool-format.md`): the recording folder goes under its prefix with
//! the same relative names, tracks and the event log first and `manifest.json` last, so a manifest in the bucket
//! means every file it names is there too. Retries are the S3 client's bounded ones; a failure after those is
//! `failed upload_failed`.

use crate::config::S3Config;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncReadExt;

/// Multipart part size.
const PART_SIZE: usize = 8 * 1024 * 1024;
/// Files at or below this go up in one PUT.
const SINGLE_PUT_MAX: u64 = PART_SIZE as u64;
/// Parts in flight per file.
const PARTS_IN_FLIGHT: usize = 4;

#[derive(Clone)]
pub struct Storage {
    store: Arc<dyn ObjectStore>,
    pub bucket: String,
}

pub struct Uploaded {
    pub key: String,
    pub bytes: u64,
}

impl Storage {
    pub fn new(cfg: &S3Config) -> Result<Self> {
        let store = AmazonS3Builder::new()
            .with_endpoint(&cfg.endpoint)
            .with_region(&cfg.region)
            .with_bucket_name(&cfg.bucket)
            .with_access_key_id(&cfg.access_key_id)
            .with_secret_access_key(&cfg.secret_access_key)
            .with_allow_http(true)
            .with_virtual_hosted_style_request(!cfg.force_path_style)
            .build()
            .context("s3 client")?;
        Ok(Self {
            store: Arc::new(store),
            bucket: cfg.bucket.clone(),
        })
    }

    /// Every file of `dir` (recursively) under `prefix/`, `manifest.json` last. Returns what went up.
    pub async fn upload_dir(&self, dir: &Path, prefix: &str) -> Result<Vec<Uploaded>> {
        let mut files = Vec::new();
        collect(dir, &mut files)?;
        files.sort();
        let manifest = dir.join("manifest.json");
        files.retain(|f| f != &manifest);
        if manifest.exists() {
            files.push(manifest);
        }
        let mut done = Vec::with_capacity(files.len());
        for file in files {
            let rel = file
                .strip_prefix(dir)
                .unwrap_or(&file)
                .to_string_lossy()
                .replace('\\', "/");
            let key = format!("{prefix}/{rel}");
            let bytes = self.upload_file(&key, &file).await?;
            done.push(Uploaded { key, bytes });
        }
        Ok(done)
    }

    pub async fn upload_file(&self, key: &str, file: &Path) -> Result<u64> {
        let size = tokio::fs::metadata(file)
            .await
            .with_context(|| format!("stat {}", file.display()))?
            .len();
        let path = ObjectPath::from(key);
        if size <= SINGLE_PUT_MAX {
            let body = tokio::fs::read(file).await?;
            self.store
                .put(&path, PutPayload::from(body))
                .await
                .with_context(|| format!("put {key}"))?;
            return Ok(size);
        }
        let upload = self
            .store
            .put_multipart(&path)
            .await
            .with_context(|| format!("create multipart {key}"))?;
        let mut writer = WriteMultipart::new_with_chunk_size(upload, PART_SIZE);
        let mut reader = tokio::fs::File::open(file).await?;
        let mut buf = vec![0u8; PART_SIZE];
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            writer.wait_for_capacity(PARTS_IN_FLIGHT).await?;
            writer.write(&buf[..n]);
        }
        writer
            .finish()
            .await
            .with_context(|| format!("complete multipart {key}"))?;
        Ok(size)
    }
}

impl Storage {
    /// Whether the manifest in the bucket already names a composite (another worker finished this recording).
    /// A missing manifest is "no": the upload is not done, or the prefix is wrong; either way not composed.
    pub async fn is_composed(&self, prefix: &str) -> Result<bool> {
        let path = ObjectPath::from(format!("{prefix}/manifest.json"));
        let bytes = match self.store.get(&path).await {
            Ok(r) => r.bytes().await?,
            Err(object_store::Error::NotFound { .. }) => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("get {path}")),
        };
        let m: crate::spool::Manifest = serde_json::from_slice(&bytes)?;
        Ok(m.composite.is_some())
    }

    /// Every object under `prefix/` into `dir` with the same relative names (a compose worker on another host).
    pub async fn download_dir(&self, prefix: &str, dir: &Path) -> Result<usize> {
        let base = ObjectPath::from(prefix);
        let mut listing = self.store.list(Some(&base));
        let mut count = 0usize;
        while let Some(meta) = listing.next().await {
            let meta = meta.context("list")?;
            let rel = meta
                .location
                .as_ref()
                .strip_prefix(&format!("{prefix}/"))
                .unwrap_or(meta.location.as_ref())
                .to_string();
            let target = dir.join(&rel);
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let bytes = self
                .store
                .get(&meta.location)
                .await
                .with_context(|| format!("get {}", meta.location))?
                .bytes()
                .await?;
            tokio::fs::write(&target, &bytes).await?;
            count += 1;
        }
        if count == 0 {
            anyhow::bail!("nothing under {prefix} in bucket {}", self.bucket);
        }
        Ok(count)
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().map(|e| e != "tmp").unwrap_or(true) {
            out.push(path);
        }
    }
    Ok(())
}
