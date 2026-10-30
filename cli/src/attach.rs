//! `moo attach <session>`: OpenCode's terminal UI for a cluster instance.
//!
//! Gets a one-time ticket, redeems it for the attach cookie, and runs a small
//! proxy on 127.0.0.1 that adds the cookie to every request (WebSocket
//! upgrades included) on its way to the hub's attach listener. Then it starts
//! `opencode attach http://127.0.0.1:<port>`, or prints the address.

use anyhow::{bail, Context, Result};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::sync::Arc;

type Body = http_body_util::combinators::BoxBody<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>>;

pub async fn run(http: &reqwest::Client, ticket_url: &str, port: u16, launch: bool) -> Result<()> {
    let r = http.get(ticket_url).send().await.context("redeeming the attach ticket")?;
    let set = r.headers().get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()).find(|v| v.starts_with("moo_attach=")).map(String::from);
    let Some(set) = set else { bail!("the hub did not accept the ticket ({})", r.status()) };
    let cookie = set.split(';').next().unwrap_or("").to_string();
    let base = reqwest::Url::parse(ticket_url)?;
    let origin = format!("{}://{}", base.scheme(), base.host_str().unwrap_or("127.0.0.1")) + &base.port().map(|p| format!(":{p}")).unwrap_or_default();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.with_context(|| format!("binding 127.0.0.1:{port}"))?;
    let local = listener.local_addr()?;
    let state = Arc::new((http.clone(), origin.clone(), cookie));
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else { continue };
            let st = state.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req| forward(st.clone(), req));
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tcp), svc).with_upgrades().await;
            });
        }
    });
    let url = format!("http://{local}");
    eprintln!("attached through {origin}; local address {url}");
    let oc = std::env::var("MOO_OPENCODE_BIN").unwrap_or_else(|_| "opencode".into());
    if launch && which(&oc) {
        // The proxy adds the hub's cookie; OpenCode needs no password of its own here.
        let status = tokio::process::Command::new(&oc).arg("attach").arg(&url).env("OPENCODE_SERVER_PASSWORD", "unused").status().await?;
        if !status.success() {
            bail!("opencode attach exited with {status}");
        }
        return Ok(());
    }
    if launch {
        eprintln!("`{oc}` not found; run: opencode attach {url}");
    }
    eprintln!("Ctrl-C to detach.");
    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn which(bin: &str) -> bool {
    if bin.contains('/') {
        return std::path::Path::new(bin).exists();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).exists()))
}

async fn forward(st: Arc<(reqwest::Client, String, String)>, mut req: Request<Incoming>) -> Result<Response<Body>, std::convert::Infallible> {
    let (http, origin, cookie) = (&st.0, &st.1, &st.2);
    let fail = |code: u16, msg: String| Ok(Response::builder().status(code).body(http_body_util::Full::new(bytes::Bytes::from(msg)).map_err(|e| match e {}).boxed()).unwrap());
    let upgrade = req.headers().contains_key(hyper::header::UPGRADE).then(|| hyper::upgrade::on(&mut req));
    let url = format!("{origin}{}", req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/"));
    let mut out = http.request(req.method().clone(), &url);
    for (k, v) in req.headers() {
        if k != hyper::header::HOST && k != hyper::header::AUTHORIZATION && k != hyper::header::COOKIE {
            out = out.header(k, v);
        }
    }
    out = out.header(hyper::header::COOKIE, cookie.as_str());
    let body = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return fail(400, e.to_string()),
    };
    let resp = match out.body(body).send().await {
        Ok(r) => r,
        Err(e) => return fail(502, format!("hub unreachable: {e}")),
    };
    let mut b = Response::builder().status(resp.status().as_u16());
    for (k, v) in resp.headers() {
        b = b.header(k, v);
    }
    if resp.status() == reqwest::StatusCode::SWITCHING_PROTOCOLS {
        if let Some(local_up) = upgrade {
            tokio::spawn(async move {
                if let (Ok(remote), Ok(local)) = (resp.upgrade().await, local_up.await) {
                    let (mut remote, mut local) = (remote, TokioIo::new(local));
                    let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
                }
            });
            return Ok(b.body(http_body_util::Empty::new().map_err(|e| match e {}).boxed()).unwrap());
        }
    }
    let stream = http_body_util::StreamBody::new(futures::StreamExt::map(resp.bytes_stream(), |r| r.map(hyper::body::Frame::data).map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)));
    Ok(b.body(BodyExt::boxed(stream)).unwrap_or_else(|_| Response::new(http_body_util::Empty::new().map_err(|e| match e {}).boxed())))
}
