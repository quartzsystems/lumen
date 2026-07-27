use std::sync::Arc;

use anyhow::{Context, Result};

use lumen_cluster::backend::cli::CliBackend as ClusterCliBackend;
use lumen_cluster::ClusterService;
use lumen_controlplane::config::Config;
use lumen_controlplane::realm::{lumen::LumenRealm, RealmRegistry};
use lumen_controlplane::{app, security, tls, AppState};
use lumen_drbd::backend::cli::CliBackend as DrbdCliBackend;
use lumen_drbd::DrbdService;
use lumen_net::backend::nm::NmBackend;
use lumen_net::backend::unavailable::UnavailableBackend;
use lumen_net::NetworkService;
use lumen_sys::backend::logind::LogindBackend;
use lumen_sys::backend::unavailable::UnavailablePower;
use lumen_sys::exec::{Exec, SystemdRun};
use lumen_sys::SysService;
use lumen_virt::backend::libvirt::LibvirtBackend;
use lumen_virt::VirtService;
use lumen_zfs::backend::cli::CliBackend;
use lumen_zfs::StorageService;

#[tokio::main]
async fn main() -> Result<()> {
    // Every domain crate, not just this one. The daemon delegates its
    // privileged work to lumen_sys and its real work to lumen_zfs, lumen_virt,
    // and lumen_net, so a filter naming only `lumen_controlplane` silently
    // drops the lines that say what was run and why it failed — "running
    // outside the sandbox", "privileged command failed", "pool created",
    // "could not open the console socket". docs/system.md sends an operator to
    // `journalctl -u lumen-controlplane` to diagnose exactly those failures,
    // and until this listed them that journal was guaranteed to be empty of
    // them. RUST_LOG still overrides the lot.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "lumen_controlplane=info,lumen_sys=info,lumen_zfs=info,lumen_virt=info,\
                 lumen_net=info,lumen_cluster=info,lumen_drbd=info,tower_http=info"
                    .into()
            }),
        )
        .init();

    let config = Config::from_env();
    tracing::info!(
        version = env!("LUMEN_VERSION"),
        listen = %config.listen,
        "starting lumen-controlplane"
    );

    // The session-signing secret lives behind a lock because joining an
    // environment swaps it for the shared one at runtime — every verifier
    // reads the cell, so the swap is one write.
    let jwt_secret = security::session_secret(security::load_or_create_secret(
        &config.state_dir.join("session-secret"),
    )?);

    // The stock realm registry: just the built-in OS realm for now. Configured
    // realms (LDAP, OIDC, …) will be loaded and appended here.
    let realms =
        RealmRegistry::new().register(Box::new(LumenRealm::new(config.pam_service.clone())));

    // The node itself, constructed first: it owns the privileged-command
    // runner every other domain borrows. Handing a command to systemd is what
    // lets `useradd` and `zpool create` happen at all without relaxing this
    // unit's ProtectSystem=strict — see lumen_sys::exec and docs/system.md.
    let exec: Arc<dyn Exec> = Arc::new(SystemdRun::new());
    let power = match LogindBackend::connect().await {
        Ok(backend) => Arc::new(backend) as Arc<dyn lumen_sys::PowerBackend>,
        Err(err) => {
            // Same policy as every other domain: an operator whose node is
            // misbehaving needs the console more than usual, and Maintenance
            // is not the only page on it.
            tracing::error!("the node's login manager is unavailable: {err}");
            Arc::new(UnavailablePower::new(err.to_string()))
        }
    };
    let sys = Arc::new(SysService::new(power, exec.clone()));

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

    // Storage. The real backend runs the supported command line, so a node
    // without the storage software swaps in the unavailable one and the
    // console comes up saying so rather than refusing to start.
    let zfs_backend: Arc<dyn lumen_zfs::backend::ZfsBackend> =
        match CliBackend::probe(exec.clone()).await {
            Ok(backend) => Arc::new(backend),
            Err(err) => {
                tracing::error!("storage is unavailable: {err}");
                Arc::new(lumen_zfs::backend::unavailable::UnavailableBackend::new(
                    err.to_string(),
                ))
            }
        };
    let storage = Arc::new(StorageService::new(zfs_backend));

    // Clustering. No probe and no unavailable fallback: a node without a
    // cluster stack is the ordinary standalone appliance, and the backend
    // answers "no environment" for it rather than being broken. Reads are
    // unprivileged; the privileged writes ride the same transient-unit path
    // as everything else. The peer channel and the service need each other —
    // the channel short-circuits calls to this node — so the channel is
    // built first and bound after.
    let peers = Arc::new(lumen_controlplane::peers::HttpPeerChannel::new(
        jwt_secret.clone(),
    ));
    let cluster = Arc::new(ClusterService::new(
        Arc::new(ClusterCliBackend::new(exec.clone())),
        peers.clone(),
        network.clone(),
        &config.state_dir,
        env!("LUMEN_VERSION"),
    ));
    peers.bind(cluster.clone());

    // Replicated storage, on top of clustering and local storage: the
    // record and the replication policy come from the cluster domain, the
    // backing zvols from the storage domain, and DRBD itself through its
    // command line — no probe for the same reason clustering has none: a
    // node without DRBD is the ordinary standalone appliance, and the
    // volume view carries the reason if it is ever asked anyway.
    let drbd = Arc::new(DrbdService::new(
        Arc::new(DrbdCliBackend::new(exec.clone())),
        peers.clone(),
        cluster.clone(),
        storage.clone(),
    ));
    peers.bind_volumes(drbd.clone());

    // Virtual machines. The hypervisor is a privileged daemon reached over its
    // own socket, exactly as networking reaches NetworkManager over the bus —
    // so the unit's hardening stays as it is. Same failure policy: an operator
    // whose hypervisor is down needs the console more than usual.
    let virt_backend: Arc<dyn lumen_virt::backend::VirtBackend> =
        match LibvirtBackend::connect().await {
            Ok(backend) => Arc::new(backend),
            Err(err) => {
                tracing::error!("virtualization is unavailable: {err}");
                Arc::new(lumen_virt::backend::unavailable::UnavailableBackend::new(
                    err.to_string(),
                ))
            }
        };
    // Constructed last, and given the other three: a machine needs a bridge
    // to attach to and a volume — local or replicated — to boot from, while
    // none of those domains has any reason to know a machine exists.
    let virt = Arc::new(VirtService::new(
        virt_backend,
        storage.clone(),
        network.clone(),
        drbd.clone(),
    ));

    // Membership gossip: push our record to every peer once a minute and
    // adopt anything newer that comes back. Quiet by design — the
    // environment view is what reports an unreachable peer.
    {
        let cluster = cluster.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                cluster.gossip_once().await;
            }
        });
    }

    let listen: std::net::SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid LUMEN_CP_LISTEN {:?}", config.listen))?;
    let no_tls = config.no_tls;
    let state_dir = config.state_dir.clone();
    let tls_cert = config.tls_cert.clone();
    let tls_key = config.tls_key.clone();

    // The TLS material, resolved before AppState because joining an
    // environment reloads the listener at runtime through the handle stored
    // there. An explicitly configured pair always wins; a node that has
    // joined an environment serves its CA-signed certificate; everything
    // else gets the minted self-signed pair, exactly as before.
    let tls_config = if no_tls {
        None
    } else {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        let (cert, key) = if tls_cert.is_some() && tls_key.is_some() {
            tls::cert_paths(&state_dir, tls_cert, tls_key)?
        } else {
            let (env_cert, env_key) = cluster.serving_cert_paths();
            if env_cert.is_file() && env_key.is_file() {
                (env_cert, env_key)
            } else {
                tls::cert_paths(&state_dir, None, None)?
            }
        };
        Some(
            axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
                .await
                .with_context(|| {
                    format!(
                        "loading TLS material {} / {}",
                        cert.display(),
                        key.display()
                    )
                })?,
        )
    };

    let router = app(Arc::new(AppState {
        config,
        jwt_secret,
        tls: tls_config.clone(),
        realms,
        sys,
        network,
        storage,
        virt,
        cluster,
        drbd,
        tasks: lumen_controlplane::tasks::TaskLog::open(state_dir.join("vm-tasks.jsonl")),
    }));

    match tls_config {
        None => {
            tracing::warn!("LUMEN_CP_NO_TLS=1 — serving plain HTTP (development only)");
            axum_server::bind(listen)
                .serve(router.into_make_service())
                .await?;
        }
        Some(tls_config) => {
            axum_server::bind_rustls(listen, tls_config)
                .serve(router.into_make_service())
                .await?;
        }
    }
    Ok(())
}
