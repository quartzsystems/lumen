//! What guest operating systems this node knows about.
//!
//! ## Why the database and not a list of our own
//!
//! The hypervisor does not restrict what a machine may run — a domain document
//! has no field for "this is Windows" that changes how it boots. What does
//! exist is **libosinfo**'s database: the shared vocabulary that
//! `virt-manager`, `virt-install`, GNOME Boxes, and Cockpit all name a guest
//! in, kept current by the distribution rather than by us. A hand-written list
//! in the console would be a second, worse copy that goes stale the week after
//! it ships.
//!
//! ## Why the files and not the library
//!
//! `osinfo-db` is a **noarch data package** — a directory of XML — while
//! `libosinfo` is a C library. Reading the directory needs nothing this crate
//! does not already have (`quick-xml` reads the domain document); linking the
//! library would put a third `-devel` package into the build root and a second
//! generated-bindings risk into a toolchain that deliberately has none. Same
//! reasoning as `lumen_zfs` choosing the command line over `libzfs`.
//!
//! A node without the database is not an error. The catalogue comes back empty
//! with the reason in it, and the console offers a free-text identifier
//! instead — a machine defines perfectly well with no recorded guest at all,
//! because the field is metadata.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::Serialize;

/// Where the distribution installs the database.
pub const OSINFO_DB_ROOT: &str = "/usr/share/osinfo";

/// One guest operating system, as the database describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OsVariant {
    /// The canonical identifier — `http://almalinux.org/almalinux/10`. This is
    /// what goes into the domain document, and it is the only field anything
    /// downstream depends on.
    pub id: String,
    /// The short form an operator would type — `almalinux10`.
    pub short_id: String,
    /// What to show — "AlmaLinux 10".
    pub name: String,
    /// The family key: `linux`, `winnt`, `macosx`, `freebsd`, …
    pub family: String,
    /// Who makes it, for grouping the long families into readable sections.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    /// Past its end of life. Still offered — an operator restoring an old
    /// machine needs it — but the console can say so.
    pub end_of_life: bool,
    /// A guest whose installer has no driver for a virtio disk or adapter, and
    /// therefore wants the driver disc in a second drive. True for Windows,
    /// which is the whole of the rule.
    pub needs_virtio_drivers: bool,
}

/// One family, with everything in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OsFamily {
    pub id: String,
    pub label: String,
    pub variants: Vec<OsVariant>,
}

/// GET /api/vms/os-catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OsCatalog {
    pub families: Vec<OsFamily>,
    /// Where it was read from, so an operator can go and look.
    pub source: String,
    /// Why there is nothing, when there is nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl OsCatalog {
    pub fn is_empty(&self) -> bool {
        self.families.is_empty()
    }

    /// Look one up by identifier, so a request naming a guest can be checked
    /// against what the node actually knows.
    pub fn variant(&self, id: &str) -> Option<&OsVariant> {
        self.families
            .iter()
            .flat_map(|family| &family.variants)
            .find(|variant| variant.id == id)
    }
}

/// The family key Windows guests carry. The one family with behaviour attached
/// to it, and only in the console: it is what turns on the driver-disc drive.
pub const WINDOWS_FAMILY: &str = "winnt";

/// How a family key reads. Anything not named here is title-cased, so a family
/// added to the database after this was written still shows up sensibly rather
/// than being dropped.
fn family_label(id: &str) -> String {
    match id {
        "winnt" => "Windows".into(),
        "win9x" => "Windows (9x)".into(),
        "linux" => "Linux".into(),
        "macosx" | "macos" => "macOS".into(),
        "freebsd" => "FreeBSD".into(),
        "netbsd" => "NetBSD".into(),
        "openbsd" => "OpenBSD".into(),
        "dragonflybsd" => "DragonFly BSD".into(),
        "solaris" => "Solaris".into(),
        "openindiana" | "illumos" => "illumos".into(),
        "os2" => "OS/2".into(),
        "dos" => "DOS".into(),
        "netware" => "NetWare".into(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "Other".into(),
            }
        }
    }
}

/// Read the database.
///
/// Blocking file work: call it from `spawn_blocking`, or once at startup. The
/// service caches the result — the database only changes when a package is
/// updated, and re-reading a thousand small files per request would be absurd.
pub fn read(root: impl AsRef<Path>) -> OsCatalog {
    let root = root.as_ref();
    let os_dir = root.join("os");
    let source = os_dir.to_string_lossy().into_owned();

    let vendor_dirs = match std::fs::read_dir(&os_dir) {
        Ok(entries) => entries,
        Err(err) => {
            return OsCatalog {
                families: Vec::new(),
                source,
                reason: Some(format!(
                    "The guest operating system database is not readable at {}: {err}. Install \
                     osinfo-db to have the console offer a list.",
                    os_dir.display()
                )),
            }
        }
    };

    let mut files: Vec<PathBuf> = Vec::new();
    for vendor in vendor_dirs.flatten() {
        let Ok(entries) = std::fs::read_dir(vendor.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("xml") {
                files.push(path);
            }
        }
    }

    let mut by_family: BTreeMap<String, Vec<OsVariant>> = BTreeMap::new();
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for variant in parse(&text) {
            by_family
                .entry(variant.family.clone())
                .or_default()
                .push(variant);
        }
    }

    let mut families: Vec<OsFamily> = by_family
        .into_iter()
        .map(|(id, mut variants)| {
            // Newest first inside each vendor, and vendors alphabetically: an
            // operator creating a machine today is looking for something
            // current, and the one they want should not be halfway down.
            variants.sort_by(|a, b| {
                a.vendor
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.vendor.as_deref().unwrap_or(""))
                    .then_with(|| {
                        b.release_date
                            .as_deref()
                            .unwrap_or("")
                            .cmp(a.release_date.as_deref().unwrap_or(""))
                    })
                    .then_with(|| a.name.cmp(&b.name))
            });
            variants.dedup_by(|a, b| a.id == b.id);
            OsFamily {
                label: family_label(&id),
                id,
                variants,
            }
        })
        .filter(|family| !family.variants.is_empty())
        .collect();

    // Linux and Windows are what an appliance actually hosts; the rest follow
    // alphabetically rather than making someone hunt for the common case.
    families.sort_by_key(|family| {
        let rank = match family.id.as_str() {
            "linux" => 0,
            "winnt" => 1,
            _ => 2,
        };
        (rank, family.label.clone())
    });

    let reason = files
        .is_empty()
        .then(|| format!("No guest descriptions found under {}.", os_dir.display()));

    OsCatalog {
        families,
        source,
        reason,
    }
}

/// Everything in one database file. A file holds one `<os>` in practice, but
/// the schema permits several and reading them all costs nothing.
fn parse(xml: &str) -> Vec<OsVariant> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut found = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut current: Option<Partial> = None;

    loop {
        let Ok(event) = reader.read_event() else {
            break;
        };
        match event {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e) => {
                let name = local_name(e.name().as_ref());
                path.push(name);
                if path.as_slice() == ["libosinfo", "os"] {
                    current = Some(Partial {
                        id: attr(e, "id"),
                        ..Partial::default()
                    });
                }
                if matches!(event, Event::Empty(_)) {
                    path.pop();
                }
            }
            Event::End(_) => {
                if path.as_slice() == ["libosinfo", "os"] {
                    if let Some(partial) = current.take() {
                        if let Some(variant) = partial.build() {
                            found.push(variant);
                        }
                    }
                }
                path.pop();
            }
            Event::Text(ref t) => {
                let Some(partial) = current.as_mut() else {
                    continue;
                };
                let Ok(value) = t.unescape() else { continue };
                let value = value.into_owned();
                // Only the direct children of <os>. A <media> or <resources>
                // block has its own <name> and <version>, and taking those
                // would describe an installer image rather than the system.
                if path.len() != 3 {
                    continue;
                }
                match path[2].as_str() {
                    "short-id" if partial.short_id.is_none() => partial.short_id = Some(value),
                    "name" if partial.name.is_none() => partial.name = Some(value),
                    "version" if partial.version.is_none() => partial.version = Some(value),
                    "family" if partial.family.is_none() => partial.family = Some(value),
                    "vendor" if partial.vendor.is_none() => partial.vendor = Some(value),
                    "release-date" if partial.release_date.is_none() => {
                        partial.release_date = Some(value)
                    }
                    "eol-date" if partial.eol_date.is_none() => partial.eol_date = Some(value),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    found
}

#[derive(Default)]
struct Partial {
    id: Option<String>,
    short_id: Option<String>,
    name: Option<String>,
    version: Option<String>,
    family: Option<String>,
    vendor: Option<String>,
    release_date: Option<String>,
    eol_date: Option<String>,
}

impl Partial {
    /// An entry with no identifier or no family is not something the console
    /// can offer or the document can record, so it is dropped rather than
    /// shown as a blank line.
    fn build(self) -> Option<OsVariant> {
        let id = self.id?;
        let family = self.family?;
        let short_id = self.short_id.unwrap_or_else(|| id.clone());
        let name = self.name.clone().unwrap_or_else(|| short_id.clone());
        Some(OsVariant {
            needs_virtio_drivers: family == WINDOWS_FAMILY,
            // Compared as ISO dates, which sort correctly as strings. An entry
            // with no end-of-life date has not reached one.
            end_of_life: self
                .eol_date
                .as_deref()
                .is_some_and(|eol| eol < today().as_str()),
            id,
            short_id,
            name,
            family,
            vendor: self.vendor,
            version: self.version,
            release_date: self.release_date,
        })
    }
}

/// Today as `YYYY-MM-DD`, from the clock, with no date library.
///
/// Only ever used to decide whether to *label* an entry as past its end of
/// life, so being a day out at a timezone boundary changes nothing anyone can
/// see, and a clock that is wildly wrong mislabels rather than misbehaves.
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs / 86_400;

    // Civil-from-days, Howard Hinnant's algorithm — exact for every date, and
    // shorter than depending on something for it.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
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

    const ALMALINUX: &str = r#"<?xml version="1.0"?>
<libosinfo version="0.0.1">
  <os id="http://almalinux.org/almalinux/10">
    <short-id>almalinux10</short-id>
    <name>AlmaLinux 10</name>
    <version>10</version>
    <vendor>AlmaLinux OS Foundation</vendor>
    <family>linux</family>
    <distro>almalinux</distro>
    <release-date>2025-05-27</release-date>
    <eol-date>2035-05-31</eol-date>
    <media arch="x86_64">
      <url>https://example.invalid/al10.iso</url>
      <name>an installer image, whose name is not the system's</name>
    </media>
  </os>
</libosinfo>"#;

    const WINDOWS: &str = r#"<?xml version="1.0"?>
<libosinfo version="0.0.1">
  <os id="http://microsoft.com/win/11">
    <short-id>win11</short-id>
    <name>Microsoft Windows 11</name>
    <version>11.0</version>
    <vendor>Microsoft Corporation</vendor>
    <family>winnt</family>
    <release-date>2021-10-05</release-date>
  </os>
</libosinfo>"#;

    const RETIRED: &str = r#"<?xml version="1.0"?>
<libosinfo version="0.0.1">
  <os id="http://microsoft.com/win/xp">
    <short-id>winxp</short-id>
    <name>Microsoft Windows XP</name>
    <vendor>Microsoft Corporation</vendor>
    <family>winnt</family>
    <release-date>2001-10-25</release-date>
    <eol-date>2014-04-08</eol-date>
  </os>
</libosinfo>"#;

    #[test]
    fn an_entry_reads_out_of_the_database_shape() {
        let found = parse(ALMALINUX);
        assert_eq!(found.len(), 1);
        let os = &found[0];
        assert_eq!(os.id, "http://almalinux.org/almalinux/10");
        assert_eq!(os.short_id, "almalinux10");
        // The nested <media> has a <name> too, and it must not win.
        assert_eq!(os.name, "AlmaLinux 10");
        assert_eq!(os.family, "linux");
        assert_eq!(os.vendor.as_deref(), Some("AlmaLinux OS Foundation"));
        assert_eq!(os.release_date.as_deref(), Some("2025-05-27"));
        assert!(!os.end_of_life);
        assert!(!os.needs_virtio_drivers);
    }

    /// The one piece of behaviour attached to a family: Windows guests are the
    /// ones that want the driver disc, and the console keys its second drive
    /// off exactly this flag.
    #[test]
    fn windows_is_the_family_that_wants_the_driver_disc() {
        let win = &parse(WINDOWS)[0];
        assert!(win.needs_virtio_drivers);
        assert_eq!(win.family, WINDOWS_FAMILY);
        assert!(!win.end_of_life);

        let xp = &parse(RETIRED)[0];
        assert!(xp.needs_virtio_drivers);
        assert!(xp.end_of_life, "an end-of-life date in the past");
    }

    #[test]
    fn a_catalogue_groups_by_family_with_the_common_ones_first() {
        let root = std::env::temp_dir().join(format!(
            "lumen-osinfo-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("os/almalinux.org")).unwrap();
        std::fs::create_dir_all(root.join("os/microsoft.com")).unwrap();
        std::fs::write(root.join("os/almalinux.org/almalinux-10.xml"), ALMALINUX).unwrap();
        std::fs::write(root.join("os/microsoft.com/win-11.xml"), WINDOWS).unwrap();
        std::fs::write(root.join("os/microsoft.com/win-xp.xml"), RETIRED).unwrap();
        // Not XML, and must be stepped over rather than breaking the read.
        std::fs::write(root.join("os/microsoft.com/README"), "not a description").unwrap();

        let catalog = read(&root);
        assert!(catalog.reason.is_none());
        assert_eq!(catalog.families.len(), 2);
        assert_eq!(catalog.families[0].id, "linux");
        assert_eq!(catalog.families[0].label, "Linux");
        assert_eq!(catalog.families[1].id, "winnt");
        assert_eq!(catalog.families[1].label, "Windows");
        // Newest first inside a vendor.
        let windows = &catalog.families[1].variants;
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].short_id, "win11");
        assert_eq!(windows[1].short_id, "winxp");

        assert!(catalog.variant("http://microsoft.com/win/11").is_some());
        assert!(catalog.variant("http://example.invalid/nope").is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    /// A node without the data package is a node the console still works on.
    #[test]
    fn a_missing_database_is_an_empty_catalogue_with_a_remedy() {
        let catalog = read("/nonexistent-lumen-osinfo");
        assert!(catalog.is_empty());
        let reason = catalog.reason.expect("it says why");
        assert!(reason.contains("osinfo-db"), "{reason}");
    }

    #[test]
    fn todays_date_is_the_shape_the_comparison_needs() {
        let today = today();
        assert_eq!(today.len(), 10, "{today}");
        assert!(today.starts_with("20"), "{today}");
        let parts: Vec<&str> = today.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert!((1..=12).contains(&parts[1].parse::<u32>().unwrap()));
        assert!((1..=31).contains(&parts[2].parse::<u32>().unwrap()));
        // The whole point: an ISO date sorts against it as a string.
        assert!("1999-01-01" < today.as_str());
        assert!("2999-01-01" > today.as_str());
    }
}
