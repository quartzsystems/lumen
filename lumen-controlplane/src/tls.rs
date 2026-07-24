//! TLS material for the :8443 listener.
//!
//! An appliance can't ship a real certificate for a hostname it doesn't know
//! yet, so first boot mints a self-signed pair into the state dir (the
//! browser warns once, like every hypervisor web UI). Admins replace it by
//! pointing LUMEN_CP_TLS_CERT/KEY at their own PEM files.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Resolve the (cert, key) PEM paths: explicit config wins, otherwise the
/// state-dir pair, minted if missing.
pub fn cert_paths(
    state_dir: &Path,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
) -> Result<(PathBuf, PathBuf)> {
    if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
        return Ok((cert, key));
    }
    let cert = state_dir.join("tls-cert.pem");
    let key = state_dir.join("tls-key.pem");
    if !cert.is_file() || !key.is_file() {
        mint_self_signed(state_dir, &cert, &key)?;
        tracing::info!("minted self-signed TLS certificate at {}", cert.display());
    }
    Ok((cert, key))
}

fn mint_self_signed(state_dir: &Path, cert_path: &Path, key_path: &Path) -> Result<()> {
    fs::create_dir_all(state_dir)
        .with_context(|| format!("creating state dir {}", state_dir.display()))?;
    // SANs cover the appliance's fixed hostname and local access; admins
    // reaching it by IP get the usual self-signed warning either way.
    let names = vec!["lumen".to_string(), "localhost".to_string()];
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(names).context("generating self-signed certificate")?;
    fs::write(cert_path, cert.pem()).with_context(|| format!("writing {}", cert_path.display()))?;
    fs::write(key_path, key_pair.serialize_pem())
        .with_context(|| format!("writing {}", key_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(key_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_once_then_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = cert_paths(dir.path(), None, None).unwrap();
        let first = fs::read(&cert).unwrap();
        let (cert2, _) = cert_paths(dir.path(), None, None).unwrap();
        assert_eq!(cert, cert2);
        assert_eq!(first, fs::read(&cert2).unwrap());
        assert!(fs::read_to_string(&key)
            .unwrap()
            .contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn explicit_paths_win() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = cert_paths(
            dir.path(),
            Some("/etc/custom/cert.pem".into()),
            Some("/etc/custom/key.pem".into()),
        )
        .unwrap();
        assert_eq!(cert, PathBuf::from("/etc/custom/cert.pem"));
        assert_eq!(key, PathBuf::from("/etc/custom/key.pem"));
        // Nothing minted when explicit paths are configured.
        assert!(!dir.path().join("tls-cert.pem").exists());
    }
}
