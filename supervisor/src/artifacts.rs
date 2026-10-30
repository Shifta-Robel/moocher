//! Artifact store (ARCHITECTURE §7): content-addressed blobs on the hub's
//! filesystem (`<data>/artifacts/b3/<aa>/<hash>`), metadata and lineage in
//! SQLite. Nodes move bytes over the `Blobs` gRPC service on their link
//! connection (mTLS or the hub's unix socket); operators over HTTP.

use crate::state::Sup;
use crate::tlslink::LinkPeer;
use anyhow::{anyhow, bail, Result};
use futures::{Stream, StreamExt};
use moocher_protocol::api::StreamEvent;
use moocher_protocol::model::*;
use moocher_protocol::proto::blobs_server::Blobs;
use moocher_protocol::proto::{BlobChunk, BlobGet, BlobPut};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tonic::{Request, Response, Status, Streaming};

const CHUNK: usize = 256 * 1024;

pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: PathBuf) -> BlobStore {
        BlobStore { root }
    }

    pub fn path(&self, hash: &str) -> PathBuf {
        self.root.join("b3").join(&hash[..2.min(hash.len())]).join(hash)
    }

    pub fn has(&self, hash: &str, size: Option<u64>) -> bool {
        std::fs::metadata(self.path(hash)).is_ok_and(|m| size.is_none_or(|s| s == m.len()))
    }

    /// Write a byte stream to the store; returns (hash, size).
    pub async fn write<S, E>(&self, mut chunks: S, max: u64) -> Result<(String, u64)>
    where
        S: Stream<Item = Result<Vec<u8>, E>> + Unpin,
        E: std::fmt::Display,
    {
        let tmp_dir = self.root.join("tmp");
        tokio::fs::create_dir_all(&tmp_dir).await?;
        let tmp = tmp_dir.join(moocher_protocol::new_id("up"));
        let mut f = tokio::fs::File::create(&tmp).await?;
        let mut h = blake3::Hasher::new();
        let mut n = 0u64;
        let res: Result<()> = async {
            while let Some(c) = chunks.next().await {
                let c = c.map_err(|e| anyhow!("upload interrupted: {e}"))?;
                n += c.len() as u64;
                if n > max {
                    bail!("artifact larger than the hub's artifacts.max_size ({max} bytes)");
                }
                h.update(&c);
                f.write_all(&c).await?;
            }
            f.sync_all().await?;
            Ok(())
        }
        .await;
        if let Err(e) = res {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        let hash = h.finalize().to_hex().to_string();
        let dest = self.path(&hash);
        tokio::fs::create_dir_all(dest.parent().unwrap()).await?;
        tokio::fs::rename(&tmp, &dest).await?;
        Ok((hash, n))
    }
}

/// What the uploader says about a blob (first `BlobChunk.header`).
#[derive(Debug, Default, Deserialize)]
pub struct PutHeader {
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub media_type: String,
    #[serde(default)]
    pub kind: String,
    /// Lets the hub skip the transfer when it already has the blob.
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Sup {
    pub fn max_artifact(&self) -> u64 {
        parse_size(&self.cfg.artifacts.max_size).unwrap_or(2 << 30)
    }

    /// Record metadata for a stored blob. The same content uploaded again keeps
    /// its first name and producer; tags and parents accumulate.
    pub fn register_artifact(&self, hash: &str, size: u64, h: &PutHeader, node: Option<&str>) -> Result<ArtifactMeta> {
        let parents: Vec<String> = h.parents.iter().filter_map(|p| parse_art_ref(p)).filter(|p| p != hash).map(|p| art_ref(&p)).collect();
        let producer = h.session.clone();
        let task_id = producer.as_deref().and_then(|s| self.db.session(s)).and_then(|s| s.task_id);
        let meta = match self.db.artifact(hash).filter(|a| a.hash == hash) {
            Some(mut a) => {
                for t in &h.tags {
                    if !a.tags.contains(t) {
                        a.tags.push(t.clone());
                    }
                }
                for p in parents {
                    if !a.parents.contains(&p) {
                        a.parents.push(p);
                    }
                }
                a
            }
            None => ArtifactMeta {
                reference: art_ref(hash),
                hash: hash.into(),
                size,
                name: if h.name.is_empty() { hash[..12].to_string() } else { h.name.clone() },
                media_type: if h.media_type.is_empty() { guess_media(&h.name, &h.kind) } else { h.media_type.clone() },
                kind: if h.kind == "tree" { "tree".into() } else { "file".into() },
                producer,
                node: node.map(String::from),
                parents,
                tags: h.tags.clone(),
                task_id,
                created_at: now_ms(),
            },
        };
        self.db.put_artifact(&meta)?;
        self.log("artifact.put", node, meta.producer.as_deref(), json!({"ref": meta.reference, "name": meta.name, "size": meta.size, "kind": meta.kind, "parents": meta.parents}));
        let _ = self.stream.send(StreamEvent::Artifact(meta.clone()));
        Ok(meta)
    }

    pub fn artifact_meta(&self, reference: &str) -> Result<ArtifactMeta> {
        let h = parse_art_ref(reference).ok_or_else(|| anyhow!("{reference:?} is not an artifact reference (art://b3/<hash>)"))?;
        self.db.artifact(&h).ok_or_else(|| anyhow!("no such artifact {reference}"))
    }

    /// Metadata with lineage both ways: what it was made from, and what was made from it.
    pub fn artifact_view(&self, reference: &str) -> Result<serde_json::Value> {
        let a = self.artifact_meta(reference)?;
        let parents: Vec<_> = a.parents.iter().filter_map(|p| self.artifact_meta(p).ok()).collect();
        let children: Vec<_> = self.db.artifacts(Some(&a.reference), None, 200).into_iter().filter(|c| c.parents.contains(&a.reference)).collect();
        Ok(json!({"artifact": a, "parents": parents, "children": children}))
    }
}

fn guess_media(name: &str, kind: &str) -> String {
    if kind == "tree" {
        return "application/x-tar+zstd".into();
    }
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "md" => "text/markdown",
        "txt" | "log" | "py" | "rs" | "js" | "ts" | "sh" | "toml" | "yaml" | "yml" | "csv" => "text/plain",
        "json" => "application/json",
        "html" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        _ => "application/octet-stream",
    }
    .into()
}

pub struct BlobService {
    pub sup: Arc<Sup>,
}

impl BlobService {
    fn caller(&self, peer: Option<&LinkPeer>, claimed: &str) -> Result<String, Status> {
        let who = match peer {
            Some(p) => self.sup.tunnel_identity(p)?,
            None => claimed.to_string(),
        };
        if !self.sup.nodes.connected(&who) {
            return Err(Status::permission_denied(format!("node {who:?} is not attached")));
        }
        Ok(who)
    }
}

type ChunkStream = Pin<Box<dyn Stream<Item = Result<BlobChunk, Status>> + Send>>;

#[tonic::async_trait]
impl Blobs for BlobService {
    async fn put_blob(&self, req: Request<Streaming<BlobChunk>>) -> Result<Response<BlobPut>, Status> {
        let peer = req.extensions().get::<LinkPeer>().cloned();
        let mut inbound = req.into_inner();
        let first = inbound.message().await?.ok_or_else(|| Status::invalid_argument("empty upload"))?;
        let h: PutHeader = serde_json::from_slice(&first.header).map_err(|e| Status::invalid_argument(format!("bad blob header: {e}")))?;
        let node = self.caller(peer.as_ref(), &h.from)?;
        if let Some(s) = &h.session {
            if self.sup.db.session(s).map(|x| x.report.node) != Some(node.clone()) {
                return Err(Status::permission_denied(format!("session {s} does not belong to node {node}")));
            }
        }
        let sup = &self.sup;
        let known = h.hash.as_deref().filter(|x| x.len() == 64 && sup.blobs.has(x, h.size)).map(String::from);
        let (hash, size) = match known {
            // Already stored: record the metadata, skip the bytes.
            Some(hash) => (hash, h.size.unwrap_or(0)),
            None => {
                let head = futures::stream::iter((!first.data.is_empty()).then_some(Ok::<Vec<u8>, Status>(first.data)));
                let rest = inbound.map(|r| r.map(|c| c.data));
                let (hash, size) = sup.blobs.write(Box::pin(head.chain(rest)), sup.max_artifact()).await.map_err(|e| Status::invalid_argument(format!("{e:#}")))?;
                if let Some(claimed) = &h.hash {
                    if claimed != &hash {
                        return Err(Status::data_loss(format!("content hash {hash} does not match the declared {claimed}")));
                    }
                }
                (hash, size)
            }
        };
        let meta = sup.register_artifact(&hash, size, &h, Some(&node)).map_err(|e| Status::internal(format!("{e:#}")))?;
        Ok(Response::new(BlobPut { meta: serde_json::to_vec(&meta).unwrap_or_default() }))
    }

    type GetBlobStream = ChunkStream;

    async fn get_blob(&self, req: Request<BlobGet>) -> Result<Response<Self::GetBlobStream>, Status> {
        let peer = req.extensions().get::<LinkPeer>().cloned();
        let r = req.into_inner();
        self.caller(peer.as_ref(), &r.from)?;
        let meta = self.sup.artifact_meta(&r.reference).map_err(|e| Status::not_found(format!("{e:#}")))?;
        let path = self.sup.blobs.path(&meta.hash);
        let mut f = tokio::fs::File::open(&path).await.map_err(|e| Status::not_found(format!("blob {} missing on the hub: {e}", meta.hash)))?;
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<BlobChunk, Status>>(8);
        let header = serde_json::to_vec(&meta).unwrap_or_default();
        tokio::spawn(async move {
            if tx.send(Ok(BlobChunk { header, data: vec![] })).await.is_err() {
                return;
            }
            let mut buf = vec![0u8; CHUNK];
            loop {
                match f.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(Ok(BlobChunk { header: vec![], data: buf[..n].to_vec() })).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(Status::internal(e.to_string()))).await;
                        return;
                    }
                }
            }
        });
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)) as ChunkStream))
    }
}
