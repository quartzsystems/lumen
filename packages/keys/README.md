# Package signing keys

## `RPM-GPG-KEY-lumen` — not present yet

The **public** half of the key Lumen's packages and repository index are signed
with. It is committed on purpose: `lumen-repo` installs it to
`/etc/pki/rpm-gpg/RPM-GPG-KEY-lumen` on every appliance, which is what lets a
node verify what it is about to install.

Until this file exists, `packages/build-rpms.sh` skips building `lumen-repo`
and says so. Every other package still builds, so a contributor can work on the
appliance without being handed key material.

The **private** half never belongs in a checkout. It lives in the repository
secrets as `LUMEN_GPG_PRIVATE_KEY`, with its passphrase as
`LUMEN_GPG_PASSPHRASE`, and is used by two workflows: `release.yml` signs each
package as it is built, and `pages.yml` signs the repository index.

[docs/updates.md](../../docs/updates.md) has the generation procedure and the
reasoning behind signing both the packages and the index.
