//! One control plane calling another: the HTTP half of the environment's
//! peer channel.
//!
//! The domain crate defines [`PeerChannel`] and this module implements it —
//! a socket, a TLS handshake, one request, one answer. Two trust shapes,
//! matching the two moments a node calls a peer:
//!
//! - **Joined**: every ordinary peer call verifies the server against the
//!   environment CA and carries a peer ticket signed with the shared session
//!   secret. Server proven by certificate, client proven by ticket — mutual,
//!   without teaching axum-server to surface client certificates (see
//!   docs/cluster.md for that decision).
//! - **Joining**: the one call a node makes *before* it has the CA. The
//!   token pins the issuer's certificate by digest, and the connection is
//!   accepted only if the served certificate hashes to the pin — so the
//!   issuer is authenticated before the token or anything it unlocks moves.
//!
//! Calls to this node short-circuit to the service directly: the local node
//! is a peer like any other to the workflow engine, and a loopback TLS
//! round-trip would only add ways to fail.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use http_body_util::BodyExt;
use lumen_cluster::environment::der_fingerprint;
use lumen_cluster::{
    ClusterError, ClusterService, EnvironmentMembership, EnvironmentNode, JoinGrant, JoinRequest,
    PeerChannel, PreflightReport, PreparePayload, TeardownPayload,
};
use lumen_drbd::{
    DrbdError, DrbdService, VolumePeers, VolumePrepare, VolumeResizeBacking, VolumeTeardown,
};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::security::{self, SessionSecret};

/// Ordinary peer calls answer in well under this; a peer that cannot is
/// reported unreachable rather than holding the environment view hostage.
const CALL_DEADLINE: Duration = Duration::from_secs(30);

/// Prepare and teardown may apply a network change and wait for its
/// confirm, so they get the same generosity as a privileged command.
const SLOW_CALL_DEADLINE: Duration = Duration::from_secs(150);

pub struct HttpPeerChannel {
    /// The environment-shared session secret peer tickets are signed with.
    secret: SessionSecret,
    /// Set once at startup, after the service exists — the service needs the
    /// channel to be built, and the channel needs the service to answer for
    /// the local node and to read the environment identity.
    service: OnceLock<Arc<ClusterService>>,
    /// The volume half, bound the same way: the drbd service takes this
    /// channel at construction and the channel short-circuits back into it
    /// for the local node.
    volumes: OnceLock<Arc<DrbdService>>,
}

impl HttpPeerChannel {
    pub fn new(secret: SessionSecret) -> Self {
        HttpPeerChannel {
            secret,
            service: OnceLock::new(),
            volumes: OnceLock::new(),
        }
    }

    /// Wire the channel to the service it short-circuits into. Called once
    /// in `main`, immediately after the service is constructed.
    pub fn bind(&self, service: Arc<ClusterService>) {
        let _ = self.service.set(service);
    }

    /// Wire the volume half, once the drbd service exists.
    pub fn bind_volumes(&self, service: Arc<DrbdService>) {
        let _ = self.volumes.set(service);
    }

    fn volume_service(&self) -> Result<&Arc<DrbdService>, DrbdError> {
        self.volumes.get().ok_or_else(|| {
            DrbdError::Backend(anyhow!(
                "the peer channel was never bound to the volume service"
            ))
        })
    }

    fn service(&self) -> Result<&Arc<ClusterService>, ClusterError> {
        self.service.get().ok_or_else(|| {
            ClusterError::Backend(anyhow!("the peer channel was never bound to the service"))
        })
    }

    fn is_local(&self, node: &EnvironmentNode) -> bool {
        self.service
            .get()
            .map(|service| service.node() == node.name)
            .unwrap_or(false)
    }

    fn peer_ticket(&self) -> Result<String, ClusterError> {
        let secret = self.secret.read().expect("secret lock").clone();
        let node = self.service()?.node().to_string();
        security::issue_peer_ticket(&secret, &node).map_err(ClusterError::Backend)
    }

    /// TLS client configuration trusting the environment CA — the joined
    /// shape.
    fn ca_client_config(&self) -> Result<rustls::ClientConfig, ClusterError> {
        let identity = self.service()?.identity()?.ok_or_else(|| {
            ClusterError::Conflict(
                "This node holds no environment credentials, so it cannot call a peer.".to_string(),
            )
        })?;
        let mut roots = rustls::RootCertStore::empty();
        for der in rustls_pemfile::certs(&mut identity.ca_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| ClusterError::Backend(anyhow!("the stored CA does not parse: {err}")))?
        {
            roots.add(der).map_err(|err| {
                ClusterError::Backend(anyhow!("the stored CA is unusable: {err}"))
            })?;
        }
        Ok(rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth())
    }

    /// TLS client configuration for the join call: accept exactly the
    /// certificate the token pinned, and nothing else.
    fn pinned_client_config(fingerprint: &str) -> rustls::ClientConfig {
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(FingerprintVerifier {
                expected: fingerprint.to_string(),
            }))
            .with_no_client_auth()
    }

    async fn call<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        address: &str,
        path: &str,
        body: &Req,
        config: rustls::ClientConfig,
        ticket: Option<String>,
        deadline: Duration,
    ) -> Result<Resp, ClusterError> {
        tokio::time::timeout(
            deadline,
            self.call_inner(address, path, body, config, ticket),
        )
        .await
        .map_err(|_| {
            ClusterError::Conflict(format!(
                "{address} did not answer within {} seconds.",
                deadline.as_secs()
            ))
        })?
    }

    async fn call_inner<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        address: &str,
        path: &str,
        body: &Req,
        config: rustls::ClientConfig,
        ticket: Option<String>,
    ) -> Result<Resp, ClusterError> {
        let failed = |err: anyhow::Error| ClusterError::Conflict(format!("{address}: {err:#}"));

        let host = address.rsplit_once(':').map(|(h, _)| h).unwrap_or(address);
        let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|_| failed(anyhow!("\"{host}\" is not a usable server name")))?;

        let tcp = tokio::net::TcpStream::connect(address)
            .await
            .with_context(|| "could not connect")
            .map_err(failed)?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(server_name, tcp)
            .await
            .with_context(|| "the TLS handshake failed")
            .map_err(failed)?;

        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tls))
                .await
                .with_context(|| "the HTTP handshake failed")
                .map_err(failed)?;
        tokio::spawn(connection);

        let payload = serde_json::to_vec(body).map_err(ClusterError::backend)?;
        let mut request = hyper::Request::post(path)
            .header(hyper::header::HOST, address)
            .header(hyper::header::CONTENT_TYPE, "application/json");
        if let Some(ticket) = ticket {
            request = request.header(hyper::header::AUTHORIZATION, format!("Bearer {ticket}"));
        }
        let request = request
            .body(http_body_util::Full::new(bytes::Bytes::from(payload)))
            .map_err(ClusterError::backend)?;

        let response = sender
            .send_request(request)
            .await
            .with_context(|| "the request failed")
            .map_err(failed)?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .with_context(|| "the answer never finished")
            .map_err(failed)?
            .to_bytes();

        if !status.is_success() {
            // The peer answers with the standard envelope; its sentence is
            // the useful one.
            let message = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .unwrap_or_else(|| format!("the peer answered {status}"));
            return Err(ClusterError::Conflict(format!("{address}: {message}")));
        }
        serde_json::from_slice(&bytes).map_err(|err| {
            ClusterError::Backend(anyhow!("{address} answered something unexpected: {err}"))
        })
    }
}

/// Accepts exactly one certificate: the one whose SHA-256 the join token
/// carried. Chain building, expiry, and names deliberately do not apply —
/// the pin *is* the trust decision, made by the operator who carried the
/// token from one console to the other.
#[derive(Debug)]
struct FingerprintVerifier {
    expected: String,
}

impl rustls::client::danger::ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if der_fingerprint(end_entity.as_ref()) == self.expected {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "the issuer's certificate does not match the token — mint a fresh token and \
                 check you are joining the node you think you are"
                    .to_string(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[async_trait]
impl PeerChannel for HttpPeerChannel {
    async fn request_join(
        &self,
        issuer: &str,
        fingerprint: &str,
        request: &JoinRequest,
    ) -> Result<JoinGrant, ClusterError> {
        self.call(
            issuer,
            "/api/peer/join",
            request,
            Self::pinned_client_config(fingerprint),
            None,
            CALL_DEADLINE,
        )
        .await
    }

    async fn preflight(&self, node: &EnvironmentNode) -> Result<PreflightReport, ClusterError> {
        if self.is_local(node) {
            return self.service()?.peer_preflight().await;
        }
        self.call(
            &node.address,
            "/api/peer/preflight",
            &serde_json::json!({}),
            self.ca_client_config()?,
            Some(self.peer_ticket()?),
            CALL_DEADLINE,
        )
        .await
    }

    async fn prepare(
        &self,
        node: &EnvironmentNode,
        payload: &PreparePayload,
    ) -> Result<(), ClusterError> {
        if self.is_local(node) {
            return self.service()?.peer_prepare(payload).await;
        }
        let _: serde_json::Value = self
            .call(
                &node.address,
                "/api/peer/cluster/prepare",
                payload,
                self.ca_client_config()?,
                Some(self.peer_ticket()?),
                SLOW_CALL_DEADLINE,
            )
            .await?;
        Ok(())
    }

    async fn start(&self, node: &EnvironmentNode) -> Result<(), ClusterError> {
        if self.is_local(node) {
            return self.service()?.peer_start().await;
        }
        let _: serde_json::Value = self
            .call(
                &node.address,
                "/api/peer/cluster/start",
                &serde_json::json!({}),
                self.ca_client_config()?,
                Some(self.peer_ticket()?),
                SLOW_CALL_DEADLINE,
            )
            .await?;
        Ok(())
    }

    async fn teardown(
        &self,
        node: &EnvironmentNode,
        payload: &TeardownPayload,
    ) -> Result<(), ClusterError> {
        if self.is_local(node) {
            return self.service()?.peer_teardown(payload).await;
        }
        let _: serde_json::Value = self
            .call(
                &node.address,
                "/api/peer/cluster/teardown",
                payload,
                self.ca_client_config()?,
                Some(self.peer_ticket()?),
                SLOW_CALL_DEADLINE,
            )
            .await?;
        Ok(())
    }

    async fn push_membership(
        &self,
        node: &EnvironmentNode,
        membership: &EnvironmentMembership,
    ) -> Result<EnvironmentMembership, ClusterError> {
        if self.is_local(node) {
            return self.service()?.receive_membership(membership.clone()).await;
        }
        self.call(
            &node.address,
            "/api/peer/membership",
            membership,
            self.ca_client_config()?,
            Some(self.peer_ticket()?),
            CALL_DEADLINE,
        )
        .await
    }

    async fn store_definition(
        &self,
        node: &EnvironmentNode,
        vmid: u32,
        xml: &str,
    ) -> Result<(), ClusterError> {
        if self.is_local(node) {
            return self.service()?.peer_store_definition(vmid, xml);
        }
        let _: serde_json::Value = self
            .call(
                &node.address,
                "/api/peer/definition/store",
                &serde_json::json!({ "vmid": vmid, "xml": xml }),
                self.ca_client_config()?,
                Some(self.peer_ticket()?),
                CALL_DEADLINE,
            )
            .await?;
        Ok(())
    }

    async fn drop_definition(&self, node: &EnvironmentNode, vmid: u32) -> Result<(), ClusterError> {
        if self.is_local(node) {
            return self.service()?.peer_drop_definition(vmid);
        }
        let _: serde_json::Value = self
            .call(
                &node.address,
                "/api/peer/definition/drop",
                &serde_json::json!({ "vmid": vmid }),
                self.ca_client_config()?,
                Some(self.peer_ticket()?),
                CALL_DEADLINE,
            )
            .await?;
        Ok(())
    }
}

/// The volume half of the channel: same socket, same tickets, same local
/// short-circuit — the drbd domain's verbs instead of the cluster's.
#[async_trait]
impl VolumePeers for HttpPeerChannel {
    async fn prepare(
        &self,
        node: &EnvironmentNode,
        payload: &VolumePrepare,
    ) -> Result<(), DrbdError> {
        if self.is_local(node) {
            return self.volume_service()?.peer_prepare(payload).await;
        }
        let _: serde_json::Value = self
            .call(
                &node.address,
                "/api/peer/volume/prepare",
                payload,
                self.ca_client_config().map_err(DrbdError::from)?,
                Some(self.peer_ticket().map_err(DrbdError::from)?),
                SLOW_CALL_DEADLINE,
            )
            .await
            .map_err(DrbdError::from)?;
        Ok(())
    }

    async fn prime(&self, node: &EnvironmentNode, resource: &str) -> Result<(), DrbdError> {
        if self.is_local(node) {
            return self.volume_service()?.peer_prime(resource).await;
        }
        let _: serde_json::Value = self
            .call(
                &node.address,
                "/api/peer/volume/prime",
                &serde_json::json!({ "resource": resource }),
                self.ca_client_config().map_err(DrbdError::from)?,
                Some(self.peer_ticket().map_err(DrbdError::from)?),
                CALL_DEADLINE,
            )
            .await
            .map_err(DrbdError::from)?;
        Ok(())
    }

    async fn teardown(
        &self,
        node: &EnvironmentNode,
        payload: &VolumeTeardown,
    ) -> Result<(), DrbdError> {
        if self.is_local(node) {
            return self.volume_service()?.peer_teardown(payload).await;
        }
        let _: serde_json::Value = self
            .call(
                &node.address,
                "/api/peer/volume/teardown",
                payload,
                self.ca_client_config().map_err(DrbdError::from)?,
                Some(self.peer_ticket().map_err(DrbdError::from)?),
                SLOW_CALL_DEADLINE,
            )
            .await
            .map_err(DrbdError::from)?;
        Ok(())
    }

    async fn resize_backing(
        &self,
        node: &EnvironmentNode,
        payload: &VolumeResizeBacking,
    ) -> Result<(), DrbdError> {
        if self.is_local(node) {
            return self.volume_service()?.peer_resize_backing(payload).await;
        }
        let _: serde_json::Value = self
            .call(
                &node.address,
                "/api/peer/volume/resize-backing",
                payload,
                self.ca_client_config().map_err(DrbdError::from)?,
                Some(self.peer_ticket().map_err(DrbdError::from)?),
                SLOW_CALL_DEADLINE,
            )
            .await
            .map_err(DrbdError::from)?;
        Ok(())
    }

    async fn grow(&self, node: &EnvironmentNode, resource: &str) -> Result<(), DrbdError> {
        if self.is_local(node) {
            return self.volume_service()?.peer_grow(resource).await;
        }
        let _: serde_json::Value = self
            .call(
                &node.address,
                "/api/peer/volume/grow",
                &serde_json::json!({ "resource": resource }),
                self.ca_client_config().map_err(DrbdError::from)?,
                Some(self.peer_ticket().map_err(DrbdError::from)?),
                CALL_DEADLINE,
            )
            .await
            .map_err(DrbdError::from)?;
        Ok(())
    }

    async fn two_primaries(
        &self,
        node: &EnvironmentNode,
        resource: &str,
        allow: bool,
    ) -> Result<(), DrbdError> {
        if self.is_local(node) {
            return self
                .volume_service()?
                .peer_two_primaries(resource, allow)
                .await;
        }
        let _: serde_json::Value = self
            .call(
                &node.address,
                "/api/peer/volume/two-primaries",
                &serde_json::json!({ "resource": resource, "allow": allow }),
                self.ca_client_config().map_err(DrbdError::from)?,
                Some(self.peer_ticket().map_err(DrbdError::from)?),
                CALL_DEADLINE,
            )
            .await
            .map_err(DrbdError::from)?;
        Ok(())
    }
}
