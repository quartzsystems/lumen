//! What the hypervisor says *this node* can do.
//!
//! The domain capabilities document is libvirt's own answer to "what may I ask
//! for here" — computed from the host CPU, the emulator binary, and the machine
//! type, and therefore right for this box rather than right in general.
//!
//! Only the processor section is read. A static table of QEMU's x86 models
//! would be easy and would also be wrong: it would offer a model this silicon
//! cannot run, the machine would define cleanly, and the failure would arrive
//! at start time as a hypervisor error rather than as a greyed-out option with
//! a reason next to it.
//!
//! Parsing lives here rather than in the backend so it can be tested without a
//! hypervisor — the same split `domain_xml` uses.

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::Serialize;

/// One named processor model the hypervisor knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CpuModelInfo {
    pub name: String,
    /// Runnable on this node's silicon. A model that is not is still listed —
    /// an operator moving a machine between unlike hosts needs to see it — but
    /// the console can say so rather than offering a start that will fail.
    pub usable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// Marked deprecated by the hypervisor. Still definable, worth saying.
    pub deprecated: bool,
}

/// GET /api/vms/cpu-models.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CpuModels {
    /// What `host-model` actually resolves to here — "EPYC-Rome",
    /// "Skylake-Server-v4". The console shows it beside the default so the
    /// default is not an unexplained word.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_model: Option<String>,
    /// Whether the hypervisor will accept `host-passthrough` at all.
    pub host_passthrough: bool,
    pub models: Vec<CpuModelInfo>,
    /// Why the list is empty, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl CpuModels {
    /// Whether a named model is one this node offers. A machine must not be
    /// defined against a model the hypervisor has never heard of — that fails
    /// at start time, long after the operator has gone.
    pub fn knows(&self, name: &str) -> bool {
        self.models.iter().any(|model| model.name == name)
    }

    pub fn usable(&self, name: &str) -> bool {
        self.models
            .iter()
            .any(|model| model.name == name && model.usable)
    }
}

/// Read the processor section of a domain capabilities document.
pub fn parse_cpu_models(xml: &str) -> CpuModels {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out = CpuModels::default();
    let mut path: Vec<String> = Vec::new();
    // Which `<mode>` we are inside: `custom` holds the named models, and
    // `host-model` holds the one name this host resolves to.
    let mut mode: Option<String> = None;
    let mut mode_supported = false;
    let mut pending: Option<CpuModelInfo> = None;

    loop {
        let Ok(event) = reader.read_event() else {
            break;
        };
        match event {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e) => {
                path.push(local_name(e.name().as_ref()));
                match path.join("/").as_str() {
                    "domainCapabilities/cpu/mode" => {
                        mode = attr(e, "name");
                        mode_supported = attr(e, "supported").as_deref() != Some("no");
                        if mode.as_deref() == Some("host-passthrough") {
                            out.host_passthrough = mode_supported;
                        }
                    }
                    "domainCapabilities/cpu/mode/model" => {
                        pending = Some(CpuModelInfo {
                            name: String::new(),
                            // Absent means the hypervisor did not compute
                            // usability, which is not the same as "no" — and
                            // refusing to offer it would be worse than
                            // offering it.
                            usable: attr(e, "usable").as_deref() != Some("no"),
                            vendor: attr(e, "vendor").filter(|v| !v.is_empty()),
                            deprecated: attr(e, "deprecated").as_deref() == Some("yes"),
                        });
                    }
                    _ => {}
                }
                if matches!(event, Event::Empty(_)) {
                    // An empty <model/> carries no name, so there is nothing
                    // to record.
                    pending = None;
                    path.pop();
                }
            }
            Event::Text(ref t) => {
                if path.join("/") != "domainCapabilities/cpu/mode/model" {
                    continue;
                }
                let Ok(value) = t.unescape() else { continue };
                let name = value.into_owned();
                if name.is_empty() {
                    continue;
                }
                match mode.as_deref() {
                    Some("custom") if mode_supported => {
                        if let Some(mut model) = pending.take() {
                            model.name = name;
                            out.models.push(model);
                        }
                    }
                    // host-model lists the models it is built from; the first
                    // is the one the hypervisor reports as this host's.
                    Some("host-model") if out.host_model.is_none() => out.host_model = Some(name),
                    _ => {}
                }
            }
            Event::End(_) => {
                if path.join("/") == "domainCapabilities/cpu/mode" {
                    mode = None;
                    mode_supported = false;
                }
                pending = None;
                path.pop();
            }
            _ => {}
        }
    }

    out.models.sort_by(|a, b| a.name.cmp(&b.name));
    out.models.dedup_by(|a, b| a.name == b.name);
    out
}

fn local_name(raw: &[u8]) -> String {
    let name = String::from_utf8_lossy(raw);
    match name.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => name.into_owned(),
    }
}

fn attr(element: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    element.attributes().flatten().find_map(|a| {
        (local_name(a.key.as_ref()) == name)
            .then(|| a.unescape_value().ok().map(|v| v.into_owned()))
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real `virsh domcapabilities` on an EL10 host.
    const CAPS: &str = r#"<domainCapabilities>
  <path>/usr/libexec/qemu-kvm</path>
  <domain>kvm</domain>
  <machine>pc-q35-rhel10.0.0</machine>
  <arch>x86_64</arch>
  <cpu>
    <mode name='host-passthrough' supported='yes'>
      <enum name='hostPassthroughMigratable'><value>on</value></enum>
    </mode>
    <mode name='maximum' supported='yes'/>
    <mode name='host-model' supported='yes'>
      <model fallback='forbid'>EPYC-Rome</model>
      <vendor>AMD</vendor>
      <feature policy='require' name='x2apic'/>
    </mode>
    <mode name='custom' supported='yes'>
      <model usable='yes' vendor='AMD' deprecated='no'>EPYC</model>
      <model usable='no' vendor='Intel' deprecated='no'>Skylake-Server</model>
      <model usable='yes' vendor='unknown' deprecated='yes'>qemu64</model>
      <model usable='yes' vendor='AMD'>EPYC-Rome</model>
    </mode>
  </cpu>
  <devices><disk supported='yes'/></devices>
</domainCapabilities>"#;

    #[test]
    fn the_named_models_come_out_with_what_this_host_can_run() {
        let caps = parse_cpu_models(CAPS);
        assert!(caps.host_passthrough);
        // The <model> inside host-model is the host's own, not a choice.
        assert_eq!(caps.host_model.as_deref(), Some("EPYC-Rome"));

        let names: Vec<&str> = caps.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["EPYC", "EPYC-Rome", "Skylake-Server", "qemu64"]);

        assert!(caps.usable("EPYC"));
        // Listed, but this silicon will not run it — the console greys it out
        // rather than letting a machine be defined that cannot start.
        assert!(caps.knows("Skylake-Server"));
        assert!(!caps.usable("Skylake-Server"));

        let deprecated = caps.models.iter().find(|m| m.name == "qemu64").unwrap();
        assert!(deprecated.deprecated);
        assert_eq!(deprecated.vendor.as_deref(), Some("unknown"));

        assert!(!caps.knows("Nehalem"));
    }

    /// A hypervisor too old to report the section, or a document that is not
    /// one at all, is an empty list rather than a panic.
    #[test]
    fn a_document_with_no_processor_section_is_simply_empty() {
        for xml in [
            "<domainCapabilities><arch>x86_64</arch></domainCapabilities>",
            "<domainCapabilities/>",
            "",
            "not xml at all",
        ] {
            let caps = parse_cpu_models(xml);
            assert!(caps.models.is_empty(), "{xml}");
            assert!(caps.host_model.is_none());
            assert!(!caps.host_passthrough);
        }
    }

    /// A mode the hypervisor says it does not support contributes nothing.
    #[test]
    fn an_unsupported_mode_offers_nothing() {
        let xml = "<domainCapabilities><cpu>\
                   <mode name='host-passthrough' supported='no'/>\
                   <mode name='custom' supported='no'><model usable='yes'>EPYC</model></mode>\
                   </cpu></domainCapabilities>";
        let caps = parse_cpu_models(xml);
        assert!(!caps.host_passthrough);
        assert!(caps.models.is_empty());
    }
}
