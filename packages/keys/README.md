# Package signing keys

## `RPM-GPG-KEY-lumen`

The **public** half of the key Lumen's packages and repository index are signed
with. It is committed on purpose: `lumen-repo` installs it to
`/etc/pki/rpm-gpg/RPM-GPG-KEY-lumen` on every appliance, which is the file
`packages/lumen.repo.in` names as `gpgkey=` and therefore what lets a node
verify what it is about to install. `pages.yml` publishes the same bytes at
`https://lumen.quartz.systems/RPM-GPG-KEY-lumen`, which is where a node being
set up by hand fetches it from — it has to trust the key before it can install
the package that carries it.

If this file is removed, `packages/build-rpms.sh` skips building `lumen-repo`
and says so. Every other package still builds, so a contributor can work on the
appliance without being handed key material.

The **private** half never belongs in a checkout. It lives in the repository
secrets as `LUMEN_GPG_PRIVATE_KEY`, with its passphrase as
`LUMEN_GPG_PASSPHRASE`, and is used by two workflows: `release.yml` signs each
package as it is built, and `pages.yml` signs the repository index.

## Replacing it

Committing a different key here republishes the site — `pages.yml` watches this
directory — but that only changes what a *new* node is handed. Appliances
already in the field hold the old key in their rpm keyring, installed from the
`lumen-repo` they have, and nothing about a site deployment reaches them. A
release signed only by a new key will not install on any of them.

So the two halves have to move in order: the new private half into the
repository secrets, the new public half committed here, and then a `lumen-repo`
update that ships the new key while still being signed by the old one. Only once
that update has reached the fleet can the old key stop signing releases.

[docs/updates.md](../../docs/updates.md) has the generation procedure and the
reasoning behind signing both the packages and the index.
