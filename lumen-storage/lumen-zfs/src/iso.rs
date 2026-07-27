//! The node's installation media library.
//!
//! One directory per pool under [`crate::model::ISO_MOUNT_ROOT`], each of them
//! the mount point of that pool's `<pool>/lumen/iso` dataset. A machine boots
//! an installer off a file in here, which is the one thing the compute domain
//! needs that is a *file* rather than a volume.
//!
//! ## Why a fixed mount point
//!
//! `ProtectSystem=strict` makes the whole hierarchy read-only inside the
//! control plane's unit, and the only way back is a `ReadWritePaths=` line in
//! a unit file written long before any pool exists. So the datasets are
//! mounted where the unit can name them — `/var/lib/lumen/iso/<pool>` — rather
//! than at the natural `/`-relative path ZFS would pick.
//!
//! ## Why the library checks whether it can see its own directory
//!
//! Creating the dataset also mounts it, and a mount made on the host while the
//! control plane is already running does not reliably appear inside the unit's
//! namespace. Rather than assume either way, [`IsoLibrary::store`] reports what
//! it can actually read: a store that exists but is not visible reads as not
//! ready, with the remedy said out loud. That turns an invisible failure into
//! a sentence an operator can act on.

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::error::{Result, ZfsError};
use crate::model::{valid_iso_name, valid_pool_name, ISO_MOUNT_ROOT};

/// A file an operator can boot a machine from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IsoView {
    /// The pool it lives on. Named `storage` because that is the word the
    /// console's picker uses, and the picker is the only thing that reads it.
    pub storage: String,
    pub name: String,
    pub size: u64,
    /// The absolute path a domain document points at.
    pub path: String,
}

/// One pool's library, whether or not it has anything in it yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IsoStoreView {
    pub storage: String,
    pub path: String,
    /// Readable and writable by the control plane right now. False means the
    /// dataset may well exist — see `reason`.
    pub ready: bool,
    /// Why it is not ready, in words with a remedy in them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Reads and writes the media library. Rooted at a path so tests can point it
/// at a temporary directory instead of the appliance's.
#[derive(Debug, Clone)]
pub struct IsoLibrary {
    root: PathBuf,
}

impl Default for IsoLibrary {
    fn default() -> Self {
        Self::new(ISO_MOUNT_ROOT)
    }
}

impl IsoLibrary {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where one pool's media lives.
    pub fn dir(&self, pool: &str) -> Result<PathBuf> {
        reject_bad_pool(pool)?;
        Ok(self.root.join(pool))
    }

    /// The absolute path of one file, with both components checked.
    ///
    /// This is the only function that turns operator-supplied strings into a
    /// path, so it is the only place traversal has to be stopped — and it is
    /// stopped by whitelisting the shape of both components rather than by
    /// trying to spot a bad one.
    pub fn path(&self, pool: &str, name: &str) -> Result<PathBuf> {
        if !valid_iso_name(name) {
            return Err(ZfsError::Conflict(format!(
                "\"{name}\" is not a usable image name. Use a single file name ending in .iso."
            )));
        }
        Ok(self.dir(pool)?.join(name))
    }

    /// What the library looks like for one pool.
    pub async fn store(&self, pool: &str) -> Result<IsoStoreView> {
        let dir = self.dir(pool)?;
        let path = dir.to_string_lossy().into_owned();
        let (ready, reason) = match fs::metadata(&dir).await {
            Ok(meta) if meta.is_dir() => (true, None),
            Ok(_) => (
                false,
                Some(format!("\"{path}\" exists but is not a directory.")),
            ),
            Err(err) if err.kind() == io::ErrorKind::NotFound => (
                false,
                Some(format!(
                    "No media library on \"{pool}\" yet. Create one, then restart the control \
                     plane so it can see the new mount: zfs create -p -o mountpoint={path} \
                     {pool}/lumen/iso"
                )),
            ),
            Err(err) => (false, Some(format!("\"{path}\" cannot be read: {err}"))),
        };
        Ok(IsoStoreView {
            storage: pool.to_string(),
            path,
            ready,
            reason,
        })
    }

    /// Every image in one pool's library, sorted by name.
    ///
    /// A library that is not there yet is empty, not an error: a node with no
    /// media is the ordinary state of a fresh install, and the store view is
    /// where the reason belongs.
    pub async fn list(&self, pool: &str) -> Result<Vec<IsoView>> {
        let dir = self.dir(pool)?;
        let mut entries = match fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(ZfsError::Backend(anyhow::Error::from(err).context(
                    format!("reading the media library at {}", dir.display()),
                )))
            }
        };

        let mut found = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(|err| {
            ZfsError::Backend(anyhow::Error::from(err).context("listing the media library"))
        })? {
            let name = entry.file_name().to_string_lossy().into_owned();
            // The same rule the writer enforces, applied to what is on disk:
            // a file an operator dropped in by hand that this cannot name
            // safely is one the console must not offer.
            if !valid_iso_name(&name) {
                continue;
            }
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            found.push(IsoView {
                storage: pool.to_string(),
                path: dir.join(&name).to_string_lossy().into_owned(),
                name,
                size: meta.len(),
            });
        }
        found.sort_by_key(|a| a.name.to_lowercase());
        Ok(found)
    }

    /// Open a file for upload, writing to a temporary name first.
    ///
    /// An interrupted upload must not leave something that looks like a
    /// bootable image, so bytes land in `<name>.part` and the file only takes
    /// its real name once [`IsoUpload::finish`] has been called. A dropped
    /// upload leaves the partial file behind on purpose — it is evidence, and
    /// the next attempt truncates it.
    pub async fn begin_upload(&self, pool: &str, name: &str) -> Result<IsoUpload> {
        let final_path = self.path(pool, name)?;
        let dir = self.dir(pool)?;
        if !fs::metadata(&dir)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            let store = self.store(pool).await?;
            return Err(ZfsError::Conflict(store.reason.unwrap_or_else(|| {
                format!("There is no media library on \"{pool}\".")
            })));
        }
        if fs::metadata(&final_path).await.is_ok() {
            return Err(ZfsError::Conflict(format!(
                "\"{name}\" is already in the \"{pool}\" media library."
            )));
        }
        let partial = final_path.with_extension("iso.part");
        let file = fs::File::create(&partial).await.map_err(|err| {
            ZfsError::Backend(
                anyhow::Error::from(err).context(format!("creating {}", partial.display())),
            )
        })?;
        Ok(IsoUpload {
            file,
            partial,
            final_path,
            written: 0,
        })
    }

    /// Remove one image. Refuses anything the name rules do not allow, so a
    /// delete can never reach outside the library.
    pub async fn delete(&self, pool: &str, name: &str) -> Result<()> {
        let path = self.path(pool, name)?;
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Err(ZfsError::NotFound(format!(
                "No image named \"{name}\" in the \"{pool}\" media library."
            ))),
            Err(err) => Err(ZfsError::Backend(
                anyhow::Error::from(err).context(format!("removing {}", path.display())),
            )),
        }
    }
}

/// An upload in progress. Bytes are streamed straight to disk — an installation
/// image is measured in gigabytes and must never be held in memory.
#[derive(Debug)]
pub struct IsoUpload {
    file: fs::File,
    partial: PathBuf,
    final_path: PathBuf,
    written: u64,
}

impl IsoUpload {
    pub async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        self.file.write_all(chunk).await.map_err(|err| {
            ZfsError::Backend(anyhow::Error::from(err).context("writing the uploaded image"))
        })?;
        self.written += chunk.len() as u64;
        Ok(())
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// Flush, then give the file its real name. An empty upload is refused
    /// rather than published: a zero-byte `.iso` is a failure that would
    /// otherwise sit in the picker looking like media.
    pub async fn finish(mut self) -> Result<u64> {
        self.file.flush().await.ok();
        self.file.sync_all().await.ok();
        drop(self.file);
        if self.written == 0 {
            let _ = fs::remove_file(&self.partial).await;
            return Err(ZfsError::Conflict(
                "The upload contained no data, so nothing was stored.".into(),
            ));
        }
        fs::rename(&self.partial, &self.final_path)
            .await
            .map_err(|err| {
                ZfsError::Backend(anyhow::Error::from(err).context(format!(
                    "publishing {} as {}",
                    self.partial.display(),
                    self.final_path.display()
                )))
            })?;
        Ok(self.written)
    }

    /// Give up and take the partial file with it.
    pub async fn abort(self) {
        drop(self.file);
        let _ = fs::remove_file(&self.partial).await;
    }
}

fn reject_bad_pool(pool: &str) -> Result<()> {
    if valid_pool_name(pool) {
        return Ok(());
    }
    Err(ZfsError::NotFound(format!(
        "No pool named \"{pool}\" on this node."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A library rooted in a fresh temporary directory, named after the test
    /// so two running at once cannot collide.
    fn library(tag: &str) -> (IsoLibrary, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "lumen-iso-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("boot")).unwrap();
        (IsoLibrary::new(&root), root)
    }

    #[tokio::test]
    async fn an_upload_only_takes_its_name_once_it_is_whole() {
        let (library, root) = library("upload");

        let mut upload = library
            .begin_upload("boot", "almalinux-10.iso")
            .await
            .unwrap();
        upload.write(b"CD001").await.unwrap();
        // Mid-upload there is a partial file and nothing the console would
        // offer as bootable media.
        assert!(root.join("boot/almalinux-10.iso.part").exists());
        assert!(library.list("boot").await.unwrap().is_empty());

        assert_eq!(upload.finish().await.unwrap(), 5);
        let listed = library.list("boot").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "almalinux-10.iso");
        assert_eq!(listed[0].size, 5);
        assert_eq!(listed[0].storage, "boot");
        assert!(listed[0].path.ends_with("almalinux-10.iso"));
        assert!(!root.join("boot/almalinux-10.iso.part").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn an_abandoned_upload_leaves_nothing_bootable_behind() {
        let (library, root) = library("abort");

        let mut upload = library.begin_upload("boot", "half.iso").await.unwrap();
        upload.write(b"partial").await.unwrap();
        upload.abort().await;
        assert!(library.list("boot").await.unwrap().is_empty());
        assert!(!root.join("boot/half.iso.part").exists());

        // And an upload with nothing in it is refused rather than published.
        let empty = library.begin_upload("boot", "empty.iso").await.unwrap();
        assert!(empty.finish().await.is_err());
        assert!(library.list("boot").await.unwrap().is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    /// The guard that stands between an operator-supplied name and the
    /// filesystem. Every one of these must be refused before a path exists.
    #[tokio::test]
    async fn a_name_that_is_really_a_path_never_becomes_one() {
        let (library, root) = library("names");
        for bad in [
            "../../../etc/passwd.iso",
            "sub/dir.iso",
            "..iso",
            "",
            "plain.txt",
        ] {
            assert!(library.path("boot", bad).is_err(), "{bad:?}");
            assert!(library.delete("boot", bad).await.is_err(), "{bad:?}");
        }
        // And a pool name that is really something else is refused too.
        assert!(library.path("../etc", "ok.iso").is_err());
        assert!(library.dir("boot/lumen").is_err());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn the_same_name_twice_is_refused_rather_than_overwriting() {
        let (library, root) = library("dup");
        let mut upload = library.begin_upload("boot", "dup.iso").await.unwrap();
        upload.write(b"first").await.unwrap();
        upload.finish().await.unwrap();

        let err = library.begin_upload("boot", "dup.iso").await.unwrap_err();
        assert!(matches!(err, ZfsError::Conflict(_)), "{err:?}");
        assert_eq!(library.list("boot").await.unwrap()[0].size, 5);

        library.delete("boot", "dup.iso").await.unwrap();
        assert!(library.list("boot").await.unwrap().is_empty());
        assert!(library.delete("boot", "dup.iso").await.is_err());

        std::fs::remove_dir_all(&root).ok();
    }

    /// A pool with no library reads as empty with a remedy, never as a crash
    /// and never as an error the console has to translate.
    #[tokio::test]
    async fn a_pool_without_a_library_says_how_to_make_one() {
        let (library, root) = library("missing");
        let store = library.store("tank").await.unwrap();
        assert!(!store.ready);
        let reason = store.reason.unwrap();
        assert!(reason.contains("tank/lumen/iso"), "{reason}");
        assert!(reason.contains("zfs create"), "{reason}");
        assert!(library.list("tank").await.unwrap().is_empty());

        let ready = library.store("boot").await.unwrap();
        assert!(ready.ready);
        assert!(ready.reason.is_none());

        std::fs::remove_dir_all(&root).ok();
    }
}
