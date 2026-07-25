//! A backend that reports why there is no hypervisor.
//!
//! Same reasoning as `lumen_net::backend::unavailable`: if the hypervisor is
//! not reachable at startup, the control plane must still come up. An operator
//! whose hypervisor is down needs the console more than usual, and "the
//! console will not start" is a far worse failure than "Virtualization is
//! unavailable on this node: <reason>".

use async_trait::async_trait;

use crate::backend::VirtBackend;
use crate::domain_caps::CpuModels;
use crate::error::{Result, VirtError};
use crate::state::{HostInfo, ObservedDomain};

pub struct UnavailableBackend {
    reason: String,
}

impl UnavailableBackend {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    fn error<T>(&self) -> Result<T> {
        Err(VirtError::Conflict(format!(
            "Virtualization is unavailable on this node: {}",
            self.reason
        )))
    }
}

#[async_trait]
impl VirtBackend for UnavailableBackend {
    async fn host(&self) -> Result<HostInfo> {
        self.error()
    }
    async fn cpu_models(&self) -> Result<CpuModels> {
        self.error()
    }
    async fn domains(&self) -> Result<Vec<ObservedDomain>> {
        self.error()
    }
    async fn domain(&self, _name: &str) -> Result<ObservedDomain> {
        self.error()
    }
    async fn define(&self, _xml: &str) -> Result<()> {
        self.error()
    }
    async fn undefine(&self, _name: &str) -> Result<()> {
        self.error()
    }
    async fn rename(&self, _name: &str, _new_name: &str) -> Result<()> {
        self.error()
    }
    async fn start(&self, _name: &str) -> Result<()> {
        self.error()
    }
    async fn shutdown(&self, _name: &str) -> Result<()> {
        self.error()
    }
    async fn destroy(&self, _name: &str) -> Result<()> {
        self.error()
    }
    async fn reboot(&self, _name: &str) -> Result<()> {
        self.error()
    }
    async fn reset(&self, _name: &str) -> Result<()> {
        self.error()
    }
    async fn set_autostart(&self, _name: &str, _on: bool) -> Result<()> {
        self.error()
    }
    async fn attach_device_live(&self, _name: &str, _device_xml: &str) -> Result<()> {
        self.error()
    }
    async fn detach_device_live(&self, _name: &str, _device_xml: &str) -> Result<()> {
        self.error()
    }
    async fn set_memory_live(&self, _name: &str, _mib: u64) -> Result<()> {
        self.error()
    }
    async fn set_vcpus_live(&self, _name: &str, _count: u32) -> Result<()> {
        self.error()
    }
    async fn set_live_metadata(&self, _name: &str, _metadata_xml: &str) -> Result<()> {
        self.error()
    }
    async fn guest_agent(&self, _name: &str, _command: &str) -> Result<String> {
        self.error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_call_explains_itself() {
        let backend = UnavailableBackend::new("could not reach the hypervisor socket");
        let err = backend.domains().await.unwrap_err();
        assert!(
            err.to_string()
                .contains("could not reach the hypervisor socket"),
            "{err}"
        );
    }
}
