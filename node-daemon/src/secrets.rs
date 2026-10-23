//! Secret reference resolution. Secrets are read on demand (so rotation needs
//! no restart) and only ever used inside keyproxy on this node.

use anyhow::{bail, Context, Result};
use ocl_protocol::config::SecretRef;

pub fn resolve(r: &SecretRef) -> Result<String> {
    let s = r.reference.as_str();
    let v = if let Some(name) = s.strip_prefix("env:") {
        std::env::var(name).with_context(|| format!("env var {name} not set"))?
    } else if let Some(path) = s.strip_prefix("file:") {
        std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?
    } else if let Some(name) = s.strip_prefix("systemd-creds:") {
        // LoadCredential=/LoadCredentialEncrypted= in the unit puts them here.
        let dir = std::env::var("CREDENTIALS_DIRECTORY").context("CREDENTIALS_DIRECTORY not set (not running under systemd with LoadCredential?)")?;
        std::fs::read_to_string(std::path::Path::new(&dir).join(name)).with_context(|| format!("credential {name}"))?
    } else if let Some(entry) = s.strip_prefix("pass:") {
        let out = std::process::Command::new("pass").arg("show").arg(entry).output().context("running pass")?;
        if !out.status.success() {
            bail!("pass show {entry} failed");
        }
        String::from_utf8(out.stdout)?.lines().next().unwrap_or_default().to_string()
    } else {
        bail!("unsupported secret ref {s:?} (use env:, file:, systemd-creds: or pass:)")
    };
    let v = v.trim().to_string();
    if v.is_empty() {
        bail!("secret {s} is empty");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn env_and_file() {
        std::env::set_var("OCL_TEST_SECRET", " abc \n");
        assert_eq!(resolve(&SecretRef { reference: "env:OCL_TEST_SECRET".into(), account: None }).unwrap(), "abc");
        let p = std::env::temp_dir().join("ocl-secret-test");
        std::fs::write(&p, "xyz\n").unwrap();
        assert_eq!(resolve(&SecretRef { reference: format!("file:{}", p.display()), account: None }).unwrap(), "xyz");
        assert!(resolve(&SecretRef { reference: "vault:x".into(), account: None }).is_err());
    }
}
