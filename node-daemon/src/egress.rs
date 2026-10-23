//! Egress routes on this node: one HTTP client per route (shared by every
//! session's keyproxy) and the public IP each route leaves from.
//!
//! The IP matters because keyless gateways count usage per IP (PHASE1_5 §3a):
//! * `direct` on a remote node: the source address the hub sees for the link
//!   (no third-party call), else an `ip_echo` probe;
//! * `proxy` / `via_node`: an `ip_echo` probe sent through that route.

use crate::cfgstore::ConfigStore;
use crate::link::LinkHandle;
use crate::relay::Dialer;
use ocl_protocol::config::Route;
use ocl_protocol::link::NodeEvent;
use ocl_protocol::model::{now_ms, EgressIp};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

pub struct Egress {
    cfg: ConfigStore,
    dialer: Option<Dialer>,
    link: LinkHandle,
    node: String,
    clients: Mutex<HashMap<String, reqwest::Client>>,
    ips: Mutex<BTreeMap<String, EgressIp>>,
}

impl Egress {
    pub fn new(cfg: ConfigStore, dialer: Option<Dialer>, link: LinkHandle) -> Egress {
        let node = cfg.get().node.name.clone();
        Egress { cfg, dialer, link, node, clients: Mutex::new(HashMap::new()), ips: Mutex::new(BTreeMap::new()) }
    }

    /// HTTP client for a route (cached; rebuilt when the route definition changes).
    pub fn client(&self, route: &Route) -> Result<reqwest::Client, String> {
        let bind = self.cfg.get().node.egress_bind.clone();
        let key = format!("{route:?}|{bind:?}");
        if let Some(c) = self.clients.lock().unwrap().get(&key) {
            return Ok(c.clone());
        }
        let mut b = reqwest::Client::builder().connect_timeout(Duration::from_secs(20));
        match route {
            Route::Direct => {
                if let Some(ip) = bind.as_deref() {
                    b = b.local_address(Some(ip.parse::<std::net::IpAddr>().map_err(|_| format!("egress_bind {ip:?} is not an IP"))?));
                }
            }
            Route::Proxy { url } => b = b.proxy(reqwest::Proxy::all(url).map_err(|e| format!("bad proxy url: {e}"))?),
            Route::ViaNode { node } => {
                let d = self.dialer.as_ref().ok_or("via_node routes are not available on this node")?;
                b = b.proxy(reqwest::Proxy::all(d.proxy_url(node)).map_err(|e| format!("relay proxy: {e}"))?);
            }
        }
        let c = b.build().map_err(|e| e.to_string())?;
        self.clients.lock().unwrap().insert(key, c.clone());
        Ok(c)
    }

    pub fn dialer(&self) -> Option<&Dialer> {
        self.dialer.as_ref()
    }

    /// Public IP of a route, if known.
    pub fn ip_of(&self, route: &str) -> Option<String> {
        self.ips.lock().unwrap().get(route).and_then(|e| e.ip.clone())
    }

    pub fn snapshot(&self) -> Vec<EgressIp> {
        self.ips.lock().unwrap().values().cloned().collect()
    }

    /// Learn the egress IP of every route an agent may use.
    pub async fn probe(&self) {
        let snap = self.cfg.get();
        let hub_ip = self.link.observed_ip();
        let mut found = BTreeMap::new();
        for name in snap.used_routes() {
            let Some(route) = snap.route(&name) else { continue };
            let mut e = EgressIp { node: self.node.clone(), route: name.clone(), ip: None, source: "unknown".into(), error: None, observed_at: now_ms() };
            // Direct traffic from a remote node leaves from the address the hub sees
            // (unless egress_bind picks another local address).
            if route == Route::Direct && snap.node.egress_bind.is_none() {
                if let Some(ip) = &hub_ip {
                    let loopback = ip.starts_with("127.") || ip == "::1";
                    if !loopback || snap.node.ip_echo.is_empty() {
                        e.ip = Some(ip.clone());
                        e.source = "hub".into();
                    }
                }
            }
            if e.ip.is_none() {
                match self.echo(&route, &snap.node.ip_echo).await {
                    Ok(ip) => {
                        e.ip = Some(ip);
                        e.source = "echo".into();
                    }
                    Err(err) => e.error = Some(err),
                }
            }
            found.insert(name, e);
        }
        let changed = {
            let mut ips = self.ips.lock().unwrap();
            let changed = ips.len() != found.len() || ips.iter().any(|(k, v)| found.get(k).map(|f| (&f.ip, &f.error)) != Some((&v.ip, &v.error)));
            *ips = found;
            changed
        };
        if changed {
            for e in self.snapshot() {
                match (&e.ip, &e.error) {
                    (Some(ip), _) => tracing::info!(route = %e.route, ip = %ip, source = %e.source, "egress IP"),
                    (None, Some(err)) => tracing::warn!(route = %e.route, error = %err, "egress IP unknown"),
                    _ => {}
                }
            }
            self.link.emit(NodeEvent::Egress { ips: self.snapshot() });
        }
    }

    async fn echo(&self, route: &Route, urls: &[String]) -> Result<String, String> {
        if urls.is_empty() {
            return Err("ip_echo is disabled".into());
        }
        let client = self.client(route)?;
        let mut last = String::new();
        for u in urls {
            match client.get(u).timeout(Duration::from_secs(10)).send().await {
                Ok(r) if r.status().is_success() => {
                    let text = r.text().await.unwrap_or_default();
                    let ip = text.trim();
                    if ip.parse::<std::net::IpAddr>().is_ok() {
                        return Ok(ip.to_string());
                    }
                    last = format!("{u} did not return an IP");
                }
                Ok(r) => last = format!("{u}: HTTP {}", r.status()),
                Err(e) => last = format!("{u}: {}", e.without_url()),
            }
        }
        Err(last)
    }
}
