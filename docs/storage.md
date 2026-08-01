# Lumen storage

Two domains share this directory. `lumen-zfs` is the node's own storage —
pools, datasets, and the local volumes a machine's disks live on — and has
carried the appliance since the beginning. `lumen-pool` (with the engine in
`lumen-fs` and the daemon in `lumen-fsd`) is the cluster's storage:
**LumenFS pooled storage**, one deduplicated pool across a cluster's
members, serving every machine disk on every member at once. That engine
has its own design document, docs/lumenfs.md, and this one does not repeat
it.

```
lumen-storage/
├── lumen-zfs/     pools, datasets, local zvols (existing)
├── lumen-cluster/ membership, quorum, fencing
├── lumen-fs/      the LumenFS engine: bytes, slices, replication
├── lumen-fsd/     the daemon serving vdisks as ublk devices
└── lumen-pool/    orchestration over LumenFS, and the compute seam
```

## The retired engine

The appliance's first replicated engine was `lumen-drbd`: one synchronously
replicated block device per volume, built from a DRBD 9 resource over a
thick zvol on each of 2–3 members. It was retired in favour of LumenFS
once the pool carried real machines on real hardware — one engine is
simpler to reason about than two behind the same seam, and no DRBD
deployment ever existed to migrate (the recorded phase-6 descope in
docs/lumenfs.md). Its design document lives in this file's git history.

Two things it defined outlived it:

- **The compute seam.** `VmVolumes` — make a disk, recognise one, destroy
  one, know where a machine using them can run, hold the migration window —
  was defined for two engines and keeps that engine-neutral shape. It lives
  in `lumen-pool/src/vm.rs` now; `VirtService` holds it as
  `Arc<dyn VmVolumes>` and has never known which engine answers.
- **Replicated machine definitions.** The HA manager's restart inventory —
  each definition carrying its home node and full domain document,
  replicated at define time because libvirt on a dead node cannot be asked
  — is engine-independent and unchanged (docs/cluster.md).

## The per-node half

`lumen-zfs` reads pools and datasets through the supported command line,
creates the zvols a machine's local disks live on, and owns the disk page's
device inventory — including the wipe guard that knows which disks are the
pool's bricks and which are leftovers a reinstall stranded
(`BrickClearance`). A machine's local disk never leaves its node; a machine
that must be able to move gets a pooled disk instead.

## Live migration's listener

The migration URI assumes libvirt listening on the Core network; the
packaging ships the firewalld service definition for it, and cluster
prepare is what enables the listener (`virtproxyd-tcp.socket`,
`auth_tcp = "none"`), teardown what disables it. Turning it on silently for
every appliance would have made the security decision nobody's; the
workflow that needs it enabling it makes it the cluster's, and the
`lumen-replication` firewalld binding — Core interfaces alone — is what
actually confines who can reach it. A standalone appliance never listens.
