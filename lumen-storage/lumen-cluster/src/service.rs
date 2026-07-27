//! The clustering domain's one entry point.
//!
//! The control plane's handlers deserialize, call one method here, and
//! serialize the answer — no validation, no corosync, nothing above this
//! line. The reads present the environment; the writes are the environment's
//! own workflows: minting a join token, joining, building a cluster,
//! destroying one. Everything that touches another node goes through the
//! [`PeerChannel`] the control plane injects, and everything that touches
//! this node goes through the backend — which is what lets all of it run
//! against the in-memory implementations under `cargo test`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::Mutex;

use crate::backend::ClusterBackend;
use crate::environment::{
    hash_secret, issue_node_certificate, mint_ca, pem_fingerprint, random_hex,
    EnvironmentMembership, EnvironmentNode, JoinToken, TOKEN_TTL_SECS,
};
use crate::error::{ClusterError, Result};
use crate::join::{
    judge_preflight, run_create, CreateProgress, JoinGrant, JoinRequest, PeerChannel,
    PreflightReport, PreflightView, PreparePayload, ProgressHandle, TeardownPayload,
};
use crate::model::Regime;
use crate::state::{hostname, ClusterState, FenceDeviceState, QuorumState, RingLink};
use crate::store::{EnvironmentStore, Identity, JoinTokenRecord};
use crate::validate::{
    validate_definition, Acknowledgements, ClusterCreate, ValidationCode, ValidationError,
};

/// GET /api/environment. The whole environment in one answer: grouped by
/// cluster, then by node — the repo's "grouped by node" shape extended one
/// level upward, exactly as the comment on `lumen_zfs::PoolsResponse`
/// promised it would be.
#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentResponse {
    /// `None` on a node that never joined an environment — today's
    /// standalone appliance, which is a valid state, not an empty one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentView>,
    pub clusters: Vec<ClusterView>,
    /// Environment nodes not assigned to any cluster: valid standalone
    /// hypervisors, listed rather than hidden. A node with no environment
    /// appears here too, alone — the console renders one shape either way.
    pub unassigned: Vec<UnassignedNodeView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentView {
    pub id: String,
    /// The membership record's last-writer-wins counter.
    pub version: u64,
    pub nodes: usize,
}

/// Overall cluster health, derived here so the console and the API can never
/// disagree about what a card's pill says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterHealth {
    Ok,
    /// Running, but something needs attention: a node down or in standby, a
    /// ring degraded, a fence device failing or never tested.
    Degraded,
    /// Not quorate, or a member is lost and not yet fenced.
    Critical,
    /// The cluster could not be asked — presented honestly rather than
    /// dropped or guessed at.
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClusterView {
    pub name: String,
    pub regime: Regime,
    pub health: ClusterHealth,
    pub quorum: QuorumState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_node: Option<String>,
    pub nodes: Vec<ClusterNodeView>,
    pub fence: FenceSummaryView,
    /// Why the cluster's state could not be read, when it could not. The
    /// nodes are then listed from the membership record with nothing claimed
    /// about them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClusterNodeView {
    pub node: String,
    pub online: bool,
    pub standby: bool,
    /// Lost and not yet fenced — the state HA waits on and the break-glass
    /// confirm exists for.
    pub unclean: bool,
    pub rings: Vec<RingLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fence: Option<FenceDeviceState>,
    /// From the membership record: where this node's own console answers,
    /// and the control plane version it last reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controlplane_version: Option<String>,
    /// This is the node answering the request.
    pub local: bool,
}

/// The fencing panel's one-line summary. IPMI is the only fence path, so a
/// failing or untested device is cluster-level news and counted here rather
/// than left for the operator to find in a table.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FenceSummaryView {
    pub devices: usize,
    pub healthy: usize,
    pub failed: usize,
    /// Devices never live-tested. Nonzero pins the persistent warning.
    pub untested: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnassignedNodeView {
    pub node: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controlplane_version: Option<String>,
    pub local: bool,
}

/// POST /api/environment/tokens.
#[derive(Debug, Clone, Serialize)]
pub struct MintedToken {
    /// The one string the operator copies to the joining node's console.
    pub token: String,
    /// Unix seconds.
    pub expires_at: u64,
    /// This mint bootstrapped the environment — the control plane reloads
    /// its TLS listener onto the new certificate when it sees this.
    pub bootstrapped: bool,
}

/// What a completed join hands back to the control plane, which owns the
/// live session secret and the TLS listener.
#[derive(Debug)]
pub struct JoinOutcome {
    pub session_secret: Vec<u8>,
}

pub struct ClusterService {
    backend: Arc<dyn ClusterBackend>,
    peers: Arc<dyn PeerChannel>,
    network: Arc<lumen_net::NetworkService>,
    store: EnvironmentStore,
    node: String,
    controlplane_version: String,
    progress: ProgressHandle,
    form_poll: Duration,
    /// Serializes writes. Two operators clicking Create at once must not
    /// race to two half-clusters.
    gate: Mutex<()>,
}

impl ClusterService {
    pub fn new(
        backend: Arc<dyn ClusterBackend>,
        peers: Arc<dyn PeerChannel>,
        network: Arc<lumen_net::NetworkService>,
        state_dir: &Path,
        controlplane_version: &str,
    ) -> Self {
        ClusterService {
            backend,
            peers,
            network,
            store: EnvironmentStore::new(state_dir),
            node: hostname(),
            controlplane_version: controlplane_version.to_string(),
            progress: ProgressHandle::default(),
            form_poll: Duration::from_secs(2),
            gate: Mutex::new(()),
        }
    }

    /// Pretend to be another node, for tests.
    pub fn with_node(mut self, node: impl Into<String>) -> Self {
        self.node = node.into();
        self
    }

    /// Seed the environment state, for tests — the shape a join would have
    /// left behind.
    pub fn with_environment(self, membership: &EnvironmentMembership) -> Self {
        self.store
            .save_membership(membership)
            .expect("test store is writable");
        self
    }

    /// Shrink the form-step poll, for tests.
    pub fn with_form_poll(mut self, poll: Duration) -> Self {
        self.form_poll = poll;
        self
    }

    pub fn node(&self) -> &str {
        &self.node
    }

    /// The environment credential material, for the control plane's TLS
    /// listener and peer client. `None` until this node bootstraps or joins.
    pub fn identity(&self) -> Result<Option<Identity>> {
        self.store.load_identity()
    }

    /// Where the environment serving certificate lives, for TLS reloads.
    pub fn serving_cert_paths(&self) -> (PathBuf, PathBuf) {
        (self.store.node_cert_path(), self.store.node_key_path())
    }

    fn membership(&self) -> Result<Option<EnvironmentMembership>> {
        self.store.load_membership()
    }

    fn require_membership(&self) -> Result<EnvironmentMembership> {
        self.membership()?.ok_or_else(|| {
            ClusterError::Conflict(
                "This node has not joined an environment. Mint a token on an environment node \
                 and join first."
                    .to_string(),
            )
        })
    }

    // --- reads ------------------------------------------------------------

    /// The whole environment: every cluster, every node, grouped.
    pub async fn environment(&self) -> Result<EnvironmentResponse> {
        let Some(membership) = self.membership()? else {
            // No environment. The node still exists and the console still
            // renders it — as the one unassigned node of an environment of
            // nobody.
            return Ok(EnvironmentResponse {
                environment: None,
                clusters: Vec::new(),
                unassigned: vec![UnassignedNodeView {
                    node: self.node.clone(),
                    address: None,
                    controlplane_version: Some(self.controlplane_version.clone()),
                    local: true,
                }],
            });
        };

        let mut clusters = Vec::new();
        for name in membership.cluster_names() {
            clusters.push(self.cluster_view(&name, &membership).await);
        }

        let unassigned = membership
            .unassigned()
            .map(|node| self.unassigned_view(node))
            .collect();

        Ok(EnvironmentResponse {
            environment: Some(EnvironmentView {
                id: membership.id.clone(),
                version: membership.version,
                nodes: membership.nodes.len(),
            }),
            clusters,
            unassigned,
        })
    }

    /// One cluster, by name. The same view the environment answer carries —
    /// the detail page is the card, opened.
    pub async fn cluster(&self, name: &str) -> Result<ClusterView> {
        let membership = self.require_membership()?;
        if !membership.cluster_names().iter().any(|c| c == name) {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{name}\" in this environment."
            )));
        }
        Ok(self.cluster_view(name, &membership).await)
    }

    // --- environment membership -------------------------------------------

    /// Mint a one-time join token. The first mint on a fresh node bootstraps
    /// the environment: an id, a CA, this node's own certificate, and a
    /// membership record of one.
    pub async fn mint_token(&self, address: &str) -> Result<MintedToken> {
        let _guard = self.gate.lock().await;

        let (membership, bootstrapped) = match self.membership()? {
            Some(membership) => (membership, false),
            None => {
                let id = random_hex();
                let (ca_pem, ca_key_pem) = mint_ca(&id)?;
                let (node_cert_pem, node_key_pem) =
                    issue_node_certificate(&id, &ca_key_pem, &self.node, address)?;
                self.store.save_identity(&Identity {
                    ca_pem,
                    ca_key_pem,
                    node_cert_pem,
                    node_key_pem,
                })?;
                let membership = EnvironmentMembership {
                    id,
                    version: 1,
                    nodes: vec![EnvironmentNode {
                        name: self.node.clone(),
                        address: address.to_string(),
                        controlplane_version: self.controlplane_version.clone(),
                        cluster: None,
                    }],
                    clusters: Vec::new(),
                };
                self.store.save_membership(&membership)?;
                tracing::info!(environment = %membership.id, "environment bootstrapped");
                (membership, true)
            }
        };

        // The address the joining node will dial is this node's recorded
        // one, not whatever the request happened to carry.
        let issuer = membership
            .node(&self.node)
            .map(|n| n.address.clone())
            .unwrap_or_else(|| address.to_string());

        let identity = self.store.load_identity()?.ok_or_else(|| {
            ClusterError::Backend(anyhow::anyhow!(
                "the environment state is missing its certificates"
            ))
        })?;

        let secret = random_hex();
        let id = random_hex()[..16].to_string();
        let expires_at = now_unix() + TOKEN_TTL_SECS;
        let mut tokens = self.store.load_tokens()?;
        tokens.retain(|t| t.expires_at > now_unix());
        tokens.push(JoinTokenRecord {
            id: id.clone(),
            secret_hash: hash_secret(&secret),
            expires_at,
        });
        self.store.save_tokens(&tokens)?;

        let token = JoinToken {
            issuer,
            id,
            secret,
            fingerprint: pem_fingerprint(&identity.node_cert_pem)?,
        };
        tracing::info!("join token minted");
        Ok(MintedToken {
            token: token.encode(),
            expires_at,
            bootstrapped,
        })
    }

    /// The issuer's half of a join: validate and consume the token, sign the
    /// newcomer's certificate, add it to the record, and hand the
    /// environment over. `session_secret` is the control plane's live
    /// web-session secret, which becomes the newcomer's too.
    pub async fn grant_join(
        &self,
        request: &JoinRequest,
        session_secret: &[u8],
    ) -> Result<JoinGrant> {
        use base64::Engine;
        let _guard = self.gate.lock().await;

        let mut membership = self.require_membership()?;

        let mut tokens = self.store.load_tokens()?;
        let now = now_unix();
        let position = tokens.iter().position(|t| {
            t.id == request.token_id
                && t.secret_hash == hash_secret(&request.secret)
                && t.expires_at > now
        });
        let Some(position) = position else {
            return Err(ClusterError::Conflict(
                "That join token is not valid here. Tokens are one-time and short-lived — mint \
                 a fresh one."
                    .to_string(),
            ));
        };
        // Consumed before anything else can fail: a token that reached a
        // failed join is spent, not reusable.
        tokens.remove(position);
        self.store.save_tokens(&tokens)?;

        if !crate::model::valid_node_name(&request.node) {
            return Err(ClusterError::Conflict(format!(
                "\"{}\" is not a usable node name.",
                request.node
            )));
        }
        if membership.node(&request.node).is_some() {
            return Err(ClusterError::Conflict(format!(
                "This environment already has a node called \"{}\". Two nodes cannot share a \
                 hostname.",
                request.node
            )));
        }

        let identity = self.store.load_identity()?.ok_or_else(|| {
            ClusterError::Backend(anyhow::anyhow!(
                "the environment state is missing its certificates"
            ))
        })?;
        let (node_cert_pem, node_key_pem) = issue_node_certificate(
            &membership.id,
            &identity.ca_key_pem,
            &request.node,
            &request.address,
        )?;

        membership.version += 1;
        membership.nodes.push(EnvironmentNode {
            name: request.node.clone(),
            address: request.address.clone(),
            controlplane_version: request.controlplane_version.clone(),
            cluster: None,
        });
        self.store.save_membership(&membership)?;
        tracing::info!(node = %request.node, "node joined the environment");

        Ok(JoinGrant {
            membership,
            ca_pem: identity.ca_pem,
            ca_key_pem: identity.ca_key_pem,
            node_cert_pem,
            node_key_pem,
            session_secret: base64::engine::general_purpose::STANDARD.encode(session_secret),
        })
    }

    /// The joining node's half: decode the pasted token, call the issuer
    /// over the fingerprint-pinned channel, and adopt what comes back. The
    /// control plane swaps its live session secret and TLS certificate from
    /// the returned outcome.
    pub async fn join(&self, token_text: &str, local_address: &str) -> Result<JoinOutcome> {
        use base64::Engine;
        let _guard = self.gate.lock().await;

        if self.store.exists() {
            return Err(ClusterError::Conflict(
                "This node is already in an environment. A node belongs to exactly one."
                    .to_string(),
            ));
        }
        let token = JoinToken::decode(token_text)?;
        let request = JoinRequest {
            token_id: token.id.clone(),
            secret: token.secret.clone(),
            node: self.node.clone(),
            address: local_address.to_string(),
            controlplane_version: self.controlplane_version.clone(),
        };
        let grant = self
            .peers
            .request_join(&token.issuer, &token.fingerprint, &request)
            .await?;

        if grant.membership.node(&self.node).is_none() {
            return Err(ClusterError::Backend(anyhow::anyhow!(
                "the issuer's answer does not include this node"
            )));
        }
        let session_secret = base64::engine::general_purpose::STANDARD
            .decode(&grant.session_secret)
            .map_err(|_| {
                ClusterError::Backend(anyhow::anyhow!("the issuer's session secret is garbled"))
            })?;

        self.store.save_identity(&Identity {
            ca_pem: grant.ca_pem,
            ca_key_pem: grant.ca_key_pem,
            node_cert_pem: grant.node_cert_pem,
            node_key_pem: grant.node_key_pem,
        })?;
        self.store.save_membership(&grant.membership)?;
        tracing::info!(environment = %grant.membership.id, "joined the environment");
        Ok(JoinOutcome { session_secret })
    }

    /// Gossip receive: reconcile a peer's record with ours and answer with
    /// the result. A node with no environment refuses — adopting a record
    /// over gossip would be a takeover, not a sync.
    pub async fn receive_membership(
        &self,
        remote: EnvironmentMembership,
    ) -> Result<EnvironmentMembership> {
        let _guard = self.gate.lock().await;
        let local = self.require_membership()?;
        let merged = EnvironmentMembership::reconcile(local, remote);
        self.store.save_membership(&merged)?;
        Ok(merged)
    }

    /// Gossip send: push our record to every peer, adopt anything newer that
    /// comes back. Best-effort by design — an unreachable peer is reported
    /// by the environment view, not by gossip failing loudly every minute.
    pub async fn gossip_once(&self) {
        let Ok(Some(mut membership)) = self.membership() else {
            return;
        };
        let peers: Vec<EnvironmentNode> = membership
            .nodes
            .iter()
            .filter(|n| n.name != self.node)
            .cloned()
            .collect();
        for peer in peers {
            match self.peers.push_membership(&peer, &membership).await {
                Ok(answer) => {
                    membership = EnvironmentMembership::reconcile(membership, answer);
                }
                Err(err) => {
                    tracing::debug!(peer = %peer.name, "gossip did not reach the peer: {err}");
                }
            }
        }
        let _guard = self.gate.lock().await;
        if let Ok(Some(local)) = self.membership() {
            let merged = EnvironmentMembership::reconcile(local, membership);
            let _ = self.store.save_membership(&merged);
        }
    }

    /// Remove an unassigned node from the environment. A cluster member
    /// leaves its cluster first; this node does not remove itself.
    pub async fn remove_node(&self, name: &str) -> Result<()> {
        let _guard = self.gate.lock().await;
        let mut membership = self.require_membership()?;
        let Some(node) = membership.node(name) else {
            return Err(ClusterError::NotFound(format!(
                "There is no node called \"{name}\" in this environment."
            )));
        };
        if let Some(cluster) = &node.cluster {
            return Err(ClusterError::Conflict(format!(
                "\"{name}\" is a member of \"{cluster}\" — remove it from the cluster first."
            )));
        }
        if name == self.node {
            return Err(ClusterError::Conflict(
                "A node does not remove itself — do this from another environment node, so \
                 there is still a console to finish from."
                    .to_string(),
            ));
        }
        membership.version += 1;
        membership.nodes.retain(|n| n.name != name);
        self.store.save_membership(&membership)?;
        tracing::info!(node = name, "node removed from the environment");
        Ok(())
    }

    // --- peer-side operations ---------------------------------------------

    /// What this node answers when a coordinator preflights it.
    pub async fn peer_preflight(&self) -> Result<PreflightReport> {
        let local = self.backend.local_preflight().await?;
        let links = self
            .network
            .observe()
            .await
            .map(|o| o.links)
            .unwrap_or_default();
        Ok(PreflightReport {
            node: self.node.clone(),
            controlplane_version: self.controlplane_version.clone(),
            hostname: self.node.clone(),
            time_synchronized: local.time_synchronized,
            time_offset_ms: local.time_offset_ms,
            already_clustered: local.already_clustered,
            links,
        })
    }

    /// Become part of a cluster: realize the Core seat through the
    /// networking domain, then write the cluster configuration. The address
    /// change is applied and confirmed in one motion — a Core address on a
    /// spare NIC cannot sever the operator, which is what the confirm window
    /// exists to protect against.
    pub async fn peer_prepare(&self, payload: &PreparePayload) -> Result<()> {
        let core = &payload.core;
        let observed = self.network.observe().await.map_err(ClusterError::from)?;
        let cidr = format!("{}/{}", core.address, core.prefix);
        let needs_address = match observed.link(&core.interface) {
            None => {
                return Err(ClusterError::Conflict(format!(
                    "This node has no link called \"{}\".",
                    core.interface
                )))
            }
            Some(link) => !link.addresses.contains(&cidr),
        };
        if needs_address {
            self.network
                .update_nic(
                    &core.interface,
                    lumen_net::service::NicPatch {
                        ip: Some(lumen_net::IpConfig::Static {
                            cidr,
                            // A Core network routes nowhere: replication and
                            // heartbeat between members, and nothing else.
                            gateway: String::new(),
                            dns: Vec::new(),
                        }),
                        mtu: Some(core.mtu),
                        ..Default::default()
                    },
                )
                .await
                .map_err(ClusterError::from)?;
            self.network
                .apply(lumen_net::Acknowledgements::default())
                .await
                .map_err(ClusterError::from)?;
            self.network.confirm().await.map_err(ClusterError::from)?;
        }
        self.backend
            .write_cluster_config(&payload.corosync_conf, &payload.authkey)
            .await
    }

    /// Start the stack — the enable half of the preset story: corosync and
    /// pacemaker are off on a fresh install and become enabled here.
    pub async fn peer_start(&self) -> Result<()> {
        self.backend.enable_stack().await
    }

    /// Put the node back exactly as it was: stack down and disabled,
    /// configuration gone, the Core address released.
    pub async fn peer_teardown(&self, payload: &TeardownPayload) -> Result<()> {
        let mut first_error: Option<ClusterError> = None;
        let mut note = |result: Result<()>| {
            if let Err(err) = result {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        };
        note(self.backend.disable_stack().await);
        note(self.backend.remove_cluster_config().await);
        if let Some(interface) = &payload.core_interface {
            let released: Result<()> = async {
                self.network
                    .update_nic(
                        interface,
                        lumen_net::service::NicPatch {
                            ip: Some(lumen_net::IpConfig::Disabled),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(ClusterError::from)?;
                self.network
                    .apply(lumen_net::Acknowledgements::default())
                    .await
                    .map_err(ClusterError::from)?;
                self.network.confirm().await.map_err(ClusterError::from)?;
                Ok(())
            }
            .await;
            note(released);
        }
        match first_error {
            None => Ok(()),
            Some(err) => Err(err),
        }
    }

    // --- cluster workflows ------------------------------------------------

    /// Ask every named node whether it could join a cluster right now — the
    /// wizard's first page, and the same judgement the create re-runs.
    pub async fn preflight(&self, nodes: &[String]) -> Result<Vec<PreflightView>> {
        let membership = self.require_membership()?;
        let mut views = Vec::new();
        for name in nodes {
            let Some(node) = membership.node(name) else {
                views.push(PreflightView {
                    node: name.clone(),
                    ok: false,
                    problems: vec![format!(
                        "\"{name}\" has not joined this environment, so it cannot join a cluster."
                    )],
                    report: None,
                });
                continue;
            };
            match self.peers.preflight(node).await {
                Ok(report) => {
                    views.push(judge_preflight(&report, name, &self.controlplane_version))
                }
                Err(err) => views.push(PreflightView {
                    node: name.clone(),
                    ok: false,
                    problems: vec![err.to_string()],
                    report: None,
                }),
            }
        }
        Ok(views)
    }

    /// Start a cluster create. Validation happens now; the workflow itself
    /// runs in the background and is polled through [`Self::create_progress`]
    /// — per node, per step, unwind included.
    pub async fn create_cluster(
        self: &Arc<Self>,
        request: ClusterCreate,
    ) -> Result<CreateProgress> {
        let _guard = self.gate.lock().await;
        if self.progress.busy() {
            return Err(ClusterError::Conflict(
                "A cluster is already being created. One at a time — watch it finish first."
                    .to_string(),
            ));
        }
        let membership = self.require_membership()?;
        let (definition, networks) = request.build().map_err(ClusterError::Invalid)?;
        let errors = validate_definition(&definition, &membership);
        if !errors.is_empty() {
            return Err(ClusterError::Invalid(errors));
        }

        // Begun here, before the spawn, so the snapshot below — and the
        // wizard's first poll — already carry the whole plan.
        self.progress.begin(crate::join::plan_progress(&definition));

        let service = self.clone();
        let version = self.controlplane_version.clone();
        let progress = self.progress.clone();
        tokio::spawn(async move {
            let result = run_create(
                definition,
                networks,
                membership,
                &version,
                service.peers.as_ref(),
                service.backend.as_ref(),
                &progress,
                service.form_poll,
            )
            .await;
            match result {
                Ok(updated) => {
                    let _guard = service.gate.lock().await;
                    if let Err(err) = service.store.save_membership(&updated) {
                        tracing::error!("the cluster formed but the record did not save: {err}");
                    }
                    drop(_guard);
                    service.gossip_once().await;
                    tracing::info!("cluster created");
                }
                Err(err) => {
                    tracing::warn!("cluster create failed and was unwound: {err}");
                }
            }
        });

        // The initial snapshot: every step pending, for the wizard to render
        // immediately.
        Ok(self
            .progress
            .snapshot()
            .expect("run_create begins the progress before its first await"))
    }

    /// The create in flight (or the last one finished), for polling.
    pub fn create_progress(&self) -> Option<CreateProgress> {
        self.progress.snapshot()
    }

    /// Destroy a cluster: every member torn down, then the record forgets
    /// it. Refused unless every member cleans up — a destroy that loses a
    /// node halfway is a stuck cluster, not a smaller one.
    pub async fn destroy_cluster(&self, name: &str, ack: Acknowledgements) -> Result<()> {
        let _guard = self.gate.lock().await;
        if !ack.may_lose_data {
            return Err(ClusterError::invalid(ValidationError::new(
                ValidationCode::UnacknowledgedDestructiveOperation,
                Some("i_understand_this_may_lose_data"),
                "Destroying a cluster stops its stack on every member. Acknowledge that to
                 proceed.",
            )));
        }
        let mut membership = self.require_membership()?;
        let Some(record) = membership.cluster_record(name).cloned() else {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{name}\" in this environment."
            )));
        };

        let members: Vec<EnvironmentNode> =
            membership.members_of(name).into_iter().cloned().collect();
        let mut failures = Vec::new();
        for node in &members {
            let payload = TeardownPayload {
                cluster: name.to_string(),
                core_interface: record
                    .networks
                    .core
                    .members
                    .iter()
                    .find(|m| m.node == node.name)
                    .map(|m| m.interface.clone()),
            };
            if let Err(err) = self.peers.teardown(node, &payload).await {
                failures.push(format!("{}: {err}", node.name));
            }
        }
        if !failures.is_empty() {
            return Err(ClusterError::Conflict(format!(
                "Not every member could be cleaned up, so the cluster is still recorded. {}",
                failures.join(" ")
            )));
        }

        membership.version += 1;
        for node in &mut membership.nodes {
            if node.cluster.as_deref() == Some(name) {
                node.cluster = None;
            }
        }
        membership.clusters.retain(|c| c.definition.name != name);
        self.store.save_membership(&membership)?;
        drop(_guard);
        self.gossip_once().await;
        tracing::info!(cluster = name, "cluster destroyed");
        Ok(())
    }

    // --- assembly ---------------------------------------------------------

    async fn cluster_view(&self, name: &str, membership: &EnvironmentMembership) -> ClusterView {
        let preferred = membership
            .cluster_record(name)
            .and_then(|r| r.definition.preferred_node.clone());
        match self.backend.cluster_state(name).await {
            Ok(state) => self.view_of(name, &state, membership, preferred),
            Err(err) => {
                // The cluster could not be asked. Say so, list its members
                // from the record, and claim nothing about them — an
                // unreachable cluster shown honestly beats one dropped from
                // the answer.
                tracing::warn!(cluster = name, "cluster state unavailable: {err}");
                let nodes = membership
                    .members_of(name)
                    .into_iter()
                    .map(|member| ClusterNodeView {
                        node: member.name.clone(),
                        online: false,
                        standby: false,
                        unclean: false,
                        rings: Vec::new(),
                        fence: None,
                        address: Some(member.address.clone()),
                        controlplane_version: Some(member.controlplane_version.clone()),
                        local: member.name == self.node,
                    })
                    .collect();
                ClusterView {
                    name: name.to_string(),
                    regime: Regime::of(membership.members_of(name).len()),
                    health: ClusterHealth::Unknown,
                    quorum: QuorumState::default(),
                    preferred_node: preferred,
                    nodes,
                    fence: FenceSummaryView::default(),
                    error: Some(err.to_string()),
                }
            }
        }
    }

    fn view_of(
        &self,
        name: &str,
        state: &ClusterState,
        membership: &EnvironmentMembership,
        preferred_node: Option<String>,
    ) -> ClusterView {
        let nodes: Vec<ClusterNodeView> = state
            .nodes
            .iter()
            .map(|node| {
                let record = membership.node(&node.name);
                ClusterNodeView {
                    node: node.name.clone(),
                    online: node.online,
                    standby: node.standby,
                    unclean: node.unclean,
                    rings: node.rings.clone(),
                    fence: state.fence_for(&node.name).cloned(),
                    address: record.map(|r| r.address.clone()),
                    controlplane_version: record.map(|r| r.controlplane_version.clone()),
                    local: node.name == self.node,
                }
            })
            .collect();

        let fence = FenceSummaryView {
            devices: state.fence_devices.len(),
            healthy: state
                .fence_devices
                .iter()
                .filter(|d| d.active && !d.failed)
                .count(),
            failed: state.fence_devices.iter().filter(|d| d.failed).count(),
            untested: state
                .fence_devices
                .iter()
                .filter(|d| d.last_test.is_none())
                .count(),
        };

        ClusterView {
            name: name.to_string(),
            regime: if state.quorum.two_node {
                Regime::TwoNode
            } else {
                Regime::of(state.nodes.len())
            },
            health: health_of(state),
            quorum: state.quorum,
            preferred_node,
            nodes,
            fence,
            error: None,
        }
    }

    fn unassigned_view(&self, node: &EnvironmentNode) -> UnassignedNodeView {
        UnassignedNodeView {
            node: node.name.clone(),
            address: Some(node.address.clone()),
            controlplane_version: Some(node.controlplane_version.clone()),
            local: node.name == self.node,
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs()
}

/// The one derivation of a cluster's health pill. Critical is reserved for
/// "data is at stake now": lost quorum, or a member lost and not yet fenced.
/// Everything an operator should fix but can schedule is Degraded — including
/// a failing or untested fence device, because IPMI is the only fence path
/// this appliance has.
fn health_of(state: &ClusterState) -> ClusterHealth {
    if !state.quorum.quorate || state.nodes.iter().any(|n| n.unclean) {
        return ClusterHealth::Critical;
    }
    let ring_degraded = state
        .nodes
        .iter()
        .any(|n| n.rings.iter().any(|r| !r.connected));
    let fence_worry = state
        .fence_devices
        .iter()
        .any(|d| d.failed || !d.active || d.last_test.is_none());
    if state.nodes.iter().any(|n| !n.online || n.standby) || ring_degraded || fence_worry {
        return ClusterHealth::Degraded;
    }
    ClusterHealth::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::{environment_membership, membership_of, MockBackend};
    use crate::join::{healthy_report, MockPeers, WorkflowPhase};
    use crate::validate::{CoreCreate, ManagementCreate, MemberCreate};

    fn test_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lumen-cluster-service-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn network() -> Arc<lumen_net::NetworkService> {
        Arc::new(lumen_net::NetworkService::new(
            Arc::new(lumen_net::backend::mock::MockBackend::appliance()),
            &test_dir("net"),
            60,
        ))
    }

    fn service_with(
        backend: MockBackend,
        peers: MockPeers,
        membership: Option<&EnvironmentMembership>,
    ) -> Arc<ClusterService> {
        let mut service = ClusterService::new(
            Arc::new(backend),
            Arc::new(peers),
            network(),
            &test_dir("state"),
            "0.3.0",
        )
        .with_node("alpha-1")
        .with_form_poll(Duration::from_millis(5));
        if let Some(membership) = membership {
            service = service.with_environment(membership);
        }
        Arc::new(service)
    }

    fn create_request(nodes: &[&str]) -> ClusterCreate {
        ClusterCreate {
            name: "alpha".into(),
            preferred_node: (nodes.len() == 2).then(|| nodes[0].to_string()),
            core: CoreCreate {
                subnet: "10.10.0.0/24".into(),
                mtu: 9000,
            },
            management: ManagementCreate {
                subnet: "192.168.10.0/24".into(),
                vip: None,
            },
            members: nodes
                .iter()
                .enumerate()
                .map(|(i, node)| MemberCreate {
                    node: (*node).into(),
                    core_interface: "nic1".into(),
                    core_address: format!("10.10.0.{}", i + 1),
                    management_interface: "nic0".into(),
                    management_address: format!("192.168.10.{}", i + 1),
                    bmc_address: format!("10.20.0.{}", i + 1),
                    bmc_username: "ADMIN".into(),
                })
                .collect(),
            external: Vec::new(),
        }
    }

    async fn finished(service: &Arc<ClusterService>) -> CreateProgress {
        for _ in 0..400 {
            if let Some(progress) = service.create_progress() {
                if progress.phase != WorkflowPhase::Running {
                    return progress;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the create never finished");
    }

    #[tokio::test]
    async fn a_node_with_no_environment_is_one_unassigned_node_not_an_error() {
        let service = service_with(MockBackend::appliance(), MockPeers::new(), None);
        let response = service.environment().await.unwrap();
        assert!(response.environment.is_none());
        assert!(response.clusters.is_empty());
        assert_eq!(response.unassigned.len(), 1);
        assert_eq!(response.unassigned[0].node, "alpha-1");
        assert!(response.unassigned[0].local);
    }

    #[tokio::test]
    async fn the_environment_answer_groups_by_cluster_then_by_node() {
        let service = service_with(
            MockBackend::environment(),
            MockPeers::new(),
            Some(&environment_membership()),
        );
        let response = service.environment().await.unwrap();

        let environment = response.environment.unwrap();
        assert_eq!(environment.nodes, 6);

        assert_eq!(response.clusters.len(), 2);
        assert_eq!(response.clusters[0].name, "alpha");
        assert_eq!(response.clusters[0].regime, Regime::TwoNode);
        assert_eq!(response.clusters[0].health, ClusterHealth::Ok);
        assert_eq!(response.clusters[0].nodes.len(), 2);
        assert_eq!(response.clusters[1].name, "beta");
        assert_eq!(response.clusters[1].regime, Regime::Quorum);
        assert_eq!(response.clusters[1].nodes.len(), 3);

        assert_eq!(response.unassigned.len(), 1);
        assert_eq!(response.unassigned[0].node, "spare-1");
        assert!(!response.unassigned[0].local);

        let locals: Vec<&str> = response
            .clusters
            .iter()
            .flat_map(|c| &c.nodes)
            .filter(|n| n.local)
            .map(|n| n.node.as_str())
            .collect();
        assert_eq!(locals, vec!["alpha-1"]);
    }

    #[tokio::test]
    async fn a_lost_unfenced_member_makes_the_cluster_critical() {
        let service = service_with(
            MockBackend::environment().with_partition("alpha", "alpha-2"),
            MockPeers::new(),
            Some(&environment_membership()),
        );
        let cluster = service.cluster("alpha").await.unwrap();
        assert_eq!(cluster.health, ClusterHealth::Critical);
        let lost = cluster.nodes.iter().find(|n| n.node == "alpha-2").unwrap();
        assert!(lost.unclean && !lost.online);
    }

    #[tokio::test]
    async fn a_failing_fence_device_degrades_the_cluster_and_is_counted() {
        let service = service_with(
            MockBackend::environment().with_fence_failure("alpha", "alpha-2"),
            MockPeers::new(),
            Some(&environment_membership()),
        );
        let cluster = service.cluster("alpha").await.unwrap();
        assert_eq!(cluster.health, ClusterHealth::Degraded);
        assert_eq!(cluster.fence.failed, 1);
        assert_eq!(cluster.fence.healthy, 1);
    }

    #[tokio::test]
    async fn an_unreachable_cluster_is_presented_not_dropped() {
        let service = service_with(
            MockBackend::environment().with_unreachable_cluster("beta"),
            MockPeers::new(),
            Some(&environment_membership()),
        );
        let response = service.environment().await.unwrap();
        assert_eq!(response.clusters.len(), 2, "beta must still be listed");
        let beta = &response.clusters[1];
        assert_eq!(beta.health, ClusterHealth::Unknown);
        assert!(beta.error.is_some());
        assert_eq!(beta.nodes.len(), 3);
        assert!(beta.nodes.iter().all(|n| !n.online && !n.unclean));
    }

    // --- environment membership -------------------------------------------

    #[tokio::test]
    async fn the_first_token_bootstraps_the_environment() {
        let service = service_with(MockBackend::appliance(), MockPeers::new(), None);
        let minted = service.mint_token("192.168.10.1:8443").await.unwrap();
        assert!(minted.bootstrapped);
        assert!(minted.token.starts_with("lumen-join/v1/"));

        let response = service.environment().await.unwrap();
        let environment = response.environment.expect("bootstrapped");
        assert_eq!(environment.nodes, 1);
        assert!(service.identity().unwrap().is_some());

        // The second mint reuses the environment.
        let again = service.mint_token("192.168.10.1:8443").await.unwrap();
        assert!(!again.bootstrapped);
    }

    #[tokio::test]
    async fn a_token_admits_a_node_once_and_only_once() {
        let issuer = service_with(MockBackend::appliance(), MockPeers::new(), None);
        let minted = issuer.mint_token("192.168.10.1:8443").await.unwrap();
        let token = JoinToken::decode(&minted.token).unwrap();

        let request = JoinRequest {
            token_id: token.id.clone(),
            secret: token.secret.clone(),
            node: "alpha-2".into(),
            address: "192.168.10.2:8443".into(),
            controlplane_version: "0.3.0".into(),
        };
        let secret = b"the-environment-secret";
        let grant = issuer.grant_join(&request, secret).await.unwrap();
        assert_eq!(grant.membership.nodes.len(), 2);
        assert!(grant.node_cert_pem.contains("BEGIN CERTIFICATE"));

        // Spent: the same token again is refused.
        let err = issuer.grant_join(&request, secret).await.unwrap_err();
        assert!(err.to_string().contains("one-time"), "{err}");
    }

    #[tokio::test]
    async fn a_wrong_secret_or_duplicate_name_is_refused() {
        let issuer = service_with(MockBackend::appliance(), MockPeers::new(), None);
        let minted = issuer.mint_token("192.168.10.1:8443").await.unwrap();
        let token = JoinToken::decode(&minted.token).unwrap();

        let mut request = JoinRequest {
            token_id: token.id.clone(),
            secret: "wrong".into(),
            node: "alpha-2".into(),
            address: "192.168.10.2:8443".into(),
            controlplane_version: "0.3.0".into(),
        };
        assert!(issuer.grant_join(&request, b"s").await.is_err());

        // A node that shares the issuer's own hostname is refused too — and
        // the refusal must not have spent the token.
        request.secret = token.secret.clone();
        request.node = "alpha-1".into();
        let err = issuer.grant_join(&request, b"s").await.unwrap_err();
        assert!(err.to_string().contains("share a hostname"), "{err}");
    }

    #[tokio::test]
    async fn joining_adopts_the_grant_whole() {
        use base64::Engine;
        // The grant a real issuer would send: alpha-1 (this test's node) has
        // just been added to an environment alpha-2 bootstrapped.
        let grant_membership = membership_of(&[("alpha-2", None), ("alpha-1", None)]);
        let peers = MockPeers::new().with_grant(JoinGrant {
            membership: grant_membership.clone(),
            ca_pem: "CA".into(),
            ca_key_pem: "CAKEY".into(),
            node_cert_pem: "CERT".into(),
            node_key_pem: "KEY".into(),
            session_secret: base64::engine::general_purpose::STANDARD.encode(b"shared-secret"),
        });
        let service = service_with(MockBackend::appliance(), peers, None);

        let token = JoinToken {
            issuer: "192.168.10.2:8443".into(),
            id: "tok".into(),
            secret: "s3cret".into(),
            fingerprint: "ab".repeat(32),
        };
        let outcome = service
            .join(&token.encode(), "192.168.10.1:8443")
            .await
            .unwrap();
        assert_eq!(outcome.session_secret, b"shared-secret");
        assert_eq!(
            service.identity().unwrap().unwrap().ca_pem,
            "CA".to_string()
        );
        let response = service.environment().await.unwrap();
        assert_eq!(response.environment.unwrap().nodes, 2);

        // Joining twice is refused: a node belongs to exactly one.
        let err = service
            .join(&token.encode(), "192.168.10.1:8443")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already"), "{err}");
    }

    #[tokio::test]
    async fn a_create_runs_to_completion_and_records_last() {
        let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
        let backend = Arc::new(MockBackend::appliance());
        let peers = MockPeers::new()
            .with_backend(backend.clone())
            .with_healthy_node("alpha-1", "0.3.0", &["nic0", "nic1"])
            .with_healthy_node("alpha-2", "0.3.0", &["nic0", "nic1"]);
        let service = Arc::new(
            ClusterService::new(
                backend.clone() as Arc<dyn ClusterBackend>,
                Arc::new(peers),
                network(),
                &test_dir("create-ok"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_form_poll(Duration::from_millis(5))
            .with_environment(&membership),
        );

        service
            .create_cluster(create_request(&["alpha-1", "alpha-2"]))
            .await
            .unwrap();
        let progress = finished(&service).await;
        assert_eq!(progress.phase, WorkflowPhase::Complete, "{progress:?}");

        // The record now carries the cluster, assignments included.
        let response = service.environment().await.unwrap();
        assert_eq!(response.clusters.len(), 1);
        assert_eq!(response.clusters[0].name, "alpha");
        assert_eq!(response.unassigned.len(), 0);

        // stonith-enabled reached the CIB.
        assert!(backend
            .properties_set()
            .contains(&("stonith-enabled".into(), "true".into())));
    }

    /// The acceptance criterion: a create failed mid-way unwinds completely —
    /// every node ends unassigned and configuration-free, and the wizard says
    /// which step failed on which node.
    #[tokio::test]
    async fn a_failed_create_unwinds_completely() {
        let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
        let backend = Arc::new(MockBackend::appliance());
        let peers = MockPeers::new()
            .with_backend(backend.clone())
            .with_healthy_node("alpha-1", "0.3.0", &["nic0", "nic1"])
            .with_healthy_node("alpha-2", "0.3.0", &["nic0", "nic1"])
            .fail_start_on("alpha-2");
        let peers = Arc::new(peers);
        let service = Arc::new(
            ClusterService::new(
                backend.clone() as Arc<dyn ClusterBackend>,
                peers.clone(),
                network(),
                &test_dir("create-unwind"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_form_poll(Duration::from_millis(5))
            .with_environment(&membership),
        );

        service
            .create_cluster(create_request(&["alpha-1", "alpha-2"]))
            .await
            .unwrap();
        let progress = finished(&service).await;
        assert_eq!(progress.phase, WorkflowPhase::Failed);
        let error = progress.error.as_deref().unwrap_or_default();
        assert!(
            error.contains("alpha-2"),
            "the error names the node: {error}"
        );

        // Both members were torn down — the one that failed included — and
        // each teardown named the Core interface to release.
        let torn = peers.torn_down();
        assert_eq!(torn.len(), 2, "{torn:?}");
        assert!(torn
            .iter()
            .all(|(_, payload)| payload.core_interface.as_deref() == Some("nic1")));

        // And the record never knew the cluster existed.
        let response = service.environment().await.unwrap();
        assert!(response.clusters.is_empty());
        assert_eq!(response.unassigned.len(), 2);
    }

    #[tokio::test]
    async fn preflight_problems_block_before_anything_is_touched() {
        let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
        let backend = Arc::new(MockBackend::appliance());
        let mut bad = healthy_report("alpha-2", "0.3.0", &["nic0", "nic1"]);
        bad.time_synchronized = false;
        let peers = MockPeers::new()
            .with_backend(backend.clone())
            .with_healthy_node("alpha-1", "0.3.0", &["nic0", "nic1"])
            .with_report("alpha-2", bad);
        let peers = Arc::new(peers);
        let service = Arc::new(
            ClusterService::new(
                backend as Arc<dyn ClusterBackend>,
                peers.clone(),
                network(),
                &test_dir("create-preflight"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_form_poll(Duration::from_millis(5))
            .with_environment(&membership),
        );

        service
            .create_cluster(create_request(&["alpha-1", "alpha-2"]))
            .await
            .unwrap();
        let progress = finished(&service).await;
        assert_eq!(progress.phase, WorkflowPhase::Failed);
        assert!(
            peers.prepared().is_empty(),
            "nothing may have been prepared"
        );
        assert!(progress
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("chrony"));
    }

    #[tokio::test]
    async fn destroying_a_cluster_needs_the_acknowledgement_and_every_member() {
        let mut membership =
            membership_of(&[("alpha-1", Some("alpha")), ("alpha-2", Some("alpha"))]);
        let request = create_request(&["alpha-1", "alpha-2"]);
        let (definition, networks) = request.build().unwrap();
        membership.clusters.push(crate::environment::ClusterRecord {
            definition,
            networks,
        });

        let peers = Arc::new(MockPeers::new());
        let service = Arc::new(
            ClusterService::new(
                Arc::new(MockBackend::environment()) as Arc<dyn ClusterBackend>,
                peers.clone(),
                network(),
                &test_dir("destroy"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_environment(&membership),
        );

        let refused = service
            .destroy_cluster("alpha", Acknowledgements::default())
            .await
            .unwrap_err();
        assert!(matches!(refused, ClusterError::Invalid(_)));

        service
            .destroy_cluster(
                "alpha",
                Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(peers.torn_down().len(), 2);
        let response = service.environment().await.unwrap();
        assert!(response.clusters.is_empty());
        assert_eq!(response.unassigned.len(), 2);
    }

    #[tokio::test]
    async fn an_unassigned_node_can_be_removed_but_a_member_cannot() {
        let membership = membership_of(&[
            ("alpha-1", None),
            ("beta-1", Some("beta")),
            ("spare-1", None),
        ]);
        let service = service_with(
            MockBackend::appliance(),
            MockPeers::new(),
            Some(&membership),
        );

        service.remove_node("spare-1").await.unwrap();
        let err = service.remove_node("beta-1").await.unwrap_err();
        assert!(err.to_string().contains("beta"), "{err}");
        let err = service.remove_node("alpha-1").await.unwrap_err();
        assert!(
            err.to_string().contains("another environment node"),
            "{err}"
        );

        let response = service.environment().await.unwrap();
        assert_eq!(
            response
                .unassigned
                .iter()
                .map(|n| n.node.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha-1"]
        );
    }

    #[tokio::test]
    async fn gossip_receive_reconciles_but_never_adopts_a_stranger() {
        let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
        let service = service_with(
            MockBackend::appliance(),
            MockPeers::new(),
            Some(&membership),
        );

        let mut newer = membership.clone();
        newer.version = 9;
        newer.nodes[1].cluster = Some("alpha".into());
        let merged = service.receive_membership(newer.clone()).await.unwrap();
        assert_eq!(merged.version, 9);

        let mut stranger = membership.clone();
        stranger.id = "someone-else".into();
        stranger.version = 99;
        let kept = service.receive_membership(stranger).await.unwrap();
        assert_eq!(kept.version, 9, "a stranger's record is never adopted");
    }
}
