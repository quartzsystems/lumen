use std::sync::Arc;

use anyhow::{Context, Result};

use lumen_controlplane::config::Config;
use lumen_controlplane::realm::{lumen::LumenRealm, RealmRegistry};
use lumen_controlplane::{app, security, tls, AppState};
use lumen_net::backend::nm::NmBackend;
use lumen_net::backend::unavailable::UnavailableBackend;
use lumen_net::NetworkService;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lumen_controlplane=info,tower_http=info".into()),
        )
        .init();

    let config = Config::from_env();
    tracing::info!(
        version = env!("LUMEN_VERSION"),
        listen = %config.listen,
        "starting lumen-controlplane"
    );

    let jwt_secret = security::load_or_create_secret(&config.state_dir.join("session-secret"))?;

    // The stock realm registry: just the built-in OS realm for now. Configured
    // realms (LDAP, OIDC, …) will be loaded and appended here.
    let realms =
        RealmRegistry::new().register(Box::new(LumenRealm::new(config.pam_service.clone())));

    // Networking. The backend is NetworkManager over the system bus: it does
    // the privileged work in its own process, so the unit's ProtectSystem=
    // strict / ProtectKernelTunables=yes hardening stays as it is. A failure
    // here is not fatal — the console must still come up to report it.
    let network = match NmBackend::connect().await {
        Ok(backend) => {
            let service = Arc::new(NetworkService::new(
                Arc::new(backend),
                &config.state_dir,
                config.net_confirm_secs,
            ));
            // Adopt a change still waiting to be confirmed, or clear up after
            // a run that died mid-apply.
            if let Err(err) = service.reconcile().await {
                tracing::error!("networking startup reconcile failed: {err}");
            }
            service
        }
        Err(err) => {
            tracing::error!("networking is unavailable: {err}");
            Arc::new(NetworkService::new(
                Arc::new(UnavailableBackend::new(err.to_string())),
                &config.state_dir,
                config.net_confirm_secs,
            ))
        }
    };

    let listen: std::net::SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid LUMEN_CP_LISTEN {:?}", config.listen))?;
    let no_tls = config.no_tls;
    let state_dir = config.state_dir.clone();
    let tls_cert = config.tls_cert.clone();
    let tls_key = config.tls_key.clone();

    let router = app(Arc::new(AppState {
        config,
        jwt_secret,
        realms,
        network,
    }));

    if no_tls {
        tracing::warn!("LUMEN_CP_NO_TLS=1 — serving plain HTTP (development only)");
        axum_server::bind(listen)
            .serve(router.into_make_service())
            .await?;
    } else {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        let (cert, key) = tls::cert_paths(&state_dir, tls_cert, tls_key)?;
        let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
            .await
            .with_context(|| {
                format!(
                    "loading TLS material {} / {}",
                    cert.display(),
                    key.display()
                )
            })?;
        axum_server::bind_rustls(listen, tls_config)
            .serve(router.into_make_service())
            .await?;
    }
    Ok(())
}
