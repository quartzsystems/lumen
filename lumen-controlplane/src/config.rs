use std::env;
use std::path::PathBuf;

/// Runtime configuration, entirely from the environment (the appliance sets
/// these in the systemd unit; developers use plain exports). Every value has
/// an appliance-appropriate default so a bare `lumen-controlplane` boots.
#[derive(Debug, Clone)]
pub struct Config {
    /// Socket the daemon listens on. The web UI and API share it.
    pub listen: String,
    /// Where generated state lives: session-signing secret, auto-minted TLS
    /// certificate. Created on first boot.
    pub state_dir: PathBuf,
    /// The static lumen-webui export served for non-/api paths.
    pub webui_dir: PathBuf,
    /// Optional externally-managed TLS certificate/key (PEM). When unset a
    /// self-signed pair is minted into the state dir on first boot.
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// Plain-HTTP mode for development only (`next dev` proxies /api here and
    /// can't speak to a self-signed cert). Never set on the appliance.
    pub no_tls: bool,
    /// PAM service name the built-in "lumen" realm authenticates against.
    /// The matching /etc/pam.d/<name> file ships in this crate's pam/ dir.
    pub pam_service: String,
    /// Session ticket lifetime in seconds.
    pub session_ttl_secs: u64,
    /// How long an applied network change waits to be confirmed before
    /// NetworkManager rolls it back on its own. Short enough that a mistake
    /// heals before anyone drives to the rack, long enough to click Confirm.
    pub net_confirm_secs: u32,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            listen: env::var("LUMEN_CP_LISTEN").unwrap_or_else(|_| "0.0.0.0:8443".into()),
            state_dir: env::var("LUMEN_CP_STATE_DIR")
                .unwrap_or_else(|_| "/var/lib/lumen-controlplane".into())
                .into(),
            webui_dir: env::var("LUMEN_CP_WEBUI_DIR")
                .unwrap_or_else(|_| "/usr/share/lumen-webui".into())
                .into(),
            tls_cert: env::var("LUMEN_CP_TLS_CERT").ok().map(Into::into),
            tls_key: env::var("LUMEN_CP_TLS_KEY").ok().map(Into::into),
            no_tls: env::var("LUMEN_CP_NO_TLS").is_ok_and(|v| v == "1"),
            pam_service: env::var("LUMEN_CP_PAM_SERVICE")
                .unwrap_or_else(|_| "lumen-controlplane".into()),
            session_ttl_secs: env::var("LUMEN_CP_SESSION_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(12 * 60 * 60),
            net_confirm_secs: env::var("LUMEN_CP_NET_CONFIRM_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
        }
    }
}
