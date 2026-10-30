//! The node's artifact cache and its client for the hub's `Blobs` service.
//!
//! Blobs are cached by hash under `<data>/cas/`. A blob made on this node is
//! cached when it is uploaded, so placing it into another workspace here needs
//! no download. The cache is trimmed to `artifact_cache` (least recently used
//! first). Bytes go over the link's gRPC channel on their own stream.

use crate::link::LinkHandle;
use anyhow::{anyhow, bail, Context, Result};
use moocher_protocol::model::*;
use moocher_protocol::proto::blobs_client::BlobsClient;
use moocher_protocol::proto::{BlobChunk, BlobGet};
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CHUNK: usize = 256 * 1024;

pub struct Cas {
    dir: PathBuf,
    link: LinkHandle,
    node: String,
    max_bytes: u64,
}

/// What to say about an upload.
#[derive(Debug, Clone, Default)]
pub struct PutInfo {
    pub name: String,
    pub kind: String,
    pub media_type: String,
    pub session: Option<String>,
    pub parents: Vec<String>,
    pub tags: Vec<String>,
}

impl Cas {
    pub fn new(dir: PathBuf, link: LinkHandle, node: &str, max_bytes: u64) -> Result<Cas> {
        std::fs::create_dir_all(dir.join("tmp"))?;
        Ok(Cas { dir, link, node: node.into(), max_bytes })
    }

    pub fn tmp_path(&self, prefix: &str) -> PathBuf {
        self.dir.join("tmp").join(moocher_protocol::new_id(prefix))
    }

    fn blob(&self, hash: &str) -> PathBuf {
        self.dir.join(hash)
    }

    fn client(&self) -> Result<BlobsClient<tonic::transport::Channel>> {
        let ch = self.link.channel().ok_or_else(|| anyhow!("supervisor unreachable (the node reconnects automatically; try again shortly)"))?;
        Ok(BlobsClient::new(ch).max_decoding_message_size(8 << 20))
    }

    /// Move a finished local file into the cache and upload it. Returns the
    /// hub's metadata.
    pub async fn put(&self, file: &Path, info: PutInfo) -> Result<ArtifactMeta> {
        let (hash, size) = {
            let p = file.to_path_buf();
            tokio::task::spawn_blocking(move || moocher_protocol::archive::hash_file(&p)).await??
        };
        let cached = self.blob(&hash);
        if !cached.exists() {
            if std::fs::rename(file, &cached).is_err() {
                std::fs::copy(file, &cached)?;
            }
        } else if file != cached {
            let _ = std::fs::remove_file(file);
        }
        let header = json!({
            "from": self.node,
            "name": info.name,
            "kind": info.kind,
            "media_type": info.media_type,
            "hash": hash,
            "size": size,
            "session": info.session,
            "parents": info.parents,
            "tags": info.tags,
        });
        let (tx, rx) = tokio::sync::mpsc::channel::<BlobChunk>(8);
        let path = cached.clone();
        let header = serde_json::to_vec(&header)?;
        tokio::spawn(async move {
            if tx.send(BlobChunk { header, data: vec![] }).await.is_err() {
                return;
            }
            let Ok(mut f) = tokio::fs::File::open(&path).await else { return };
            let mut buf = vec![0u8; CHUNK];
            while let Ok(n) = f.read(&mut buf).await {
                // The hub may answer early (it already has the blob) and stop reading.
                if n == 0 || tx.send(BlobChunk { header: vec![], data: buf[..n].to_vec() }).await.is_err() {
                    break;
                }
            }
        });
        let reply = self.client()?.put_blob(tokio_stream::wrappers::ReceiverStream::new(rx)).await.map_err(|s| anyhow!("upload refused: {}", s.message()))?.into_inner();
        let meta: ArtifactMeta = serde_json::from_slice(&reply.meta)?;
        self.touch(&hash);
        self.trim();
        Ok(meta)
    }

    /// Local path of a blob, downloading it if needed (verified against its hash).
    pub async fn get(&self, reference: &str) -> Result<(PathBuf, ArtifactMeta)> {
        let hash = parse_art_ref(reference).ok_or_else(|| anyhow!("{reference:?} is not an artifact reference (art://b3/<hash>)"))?;
        let mut stream = self
            .client()?
            .get_blob(BlobGet { reference: hash.clone(), from: self.node.clone() })
            .await
            .map_err(|s| anyhow!("{}", s.message()))?
            .into_inner();
        let first = stream.message().await?.ok_or_else(|| anyhow!("empty reply from the hub"))?;
        let meta: ArtifactMeta = serde_json::from_slice(&first.header).context("bad artifact header")?;
        let cached = self.blob(&meta.hash);
        if cached.exists() {
            self.touch(&meta.hash);
            return Ok((cached, meta)); // the stream is dropped; the hub stops sending
        }
        let tmp = self.tmp_path("dl");
        let mut f = tokio::fs::File::create(&tmp).await?;
        let mut h = blake3::Hasher::new();
        while let Some(c) = stream.message().await? {
            h.update(&c.data);
            f.write_all(&c.data).await?;
        }
        f.sync_all().await?;
        let got = h.finalize().to_hex().to_string();
        if got != meta.hash {
            let _ = tokio::fs::remove_file(&tmp).await;
            bail!("download of {} was corrupted (hash {got})", meta.reference);
        }
        tokio::fs::rename(&tmp, &cached).await?;
        self.trim();
        Ok((cached, meta))
    }

    fn touch(&self, hash: &str) {
        let p = self.blob(hash);
        if let Ok(f) = std::fs::File::options().append(true).open(&p) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
    }

    /// Drop least recently used blobs beyond the cache size.
    fn trim(&self) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return };
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
            .flatten()
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                m.is_file().then(|| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), e.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        if total <= self.max_bytes {
            return;
        }
        files.sort();
        for (_, len, p) in files {
            if total <= self.max_bytes {
                break;
            }
            if std::fs::remove_file(&p).is_ok() {
                total -= len;
            }
        }
    }
}
