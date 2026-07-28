# Pooled storage beyond two nodes

A design for what replaces per-volume DRBD placement once a cluster is larger
than two nodes, and for the pooled-capacity model an operator expects when
they say "vSAN".

This document does not describe anything that is built. `docs/storage.md` is
what exists: local ZFS pools per node, and replicated volumes as DRBD 9
resources over a thick zvol on 2–3 named members. That model is the starting
point and the reason the answer differs by cluster size.

---

## What is actually being asked for

"Pool the drives across the nodes into one large pool" is three separate
requests wearing one sentence, and they have different costs:

1. **One capacity figure for the cluster.** The console adds up what the
   members have and reports it as one number.
2. **Placement without naming nodes.** The operator asks for a volume with a
   redundancy level; something else decides which members hold it, and
   rebalances when members come and go.
3. **A volume larger than any one member.** A single address space striped
   across nodes, so a 40 TiB volume can live on a cluster of 20 TiB members.

The first is a display problem and is nearly free. The second is a placement
engine. Only the third requires a distributed object store, and it is the one
that makes Ceph unavoidable — everything below turns on that.

DRBD, at any node count, replicates *whole* devices onto *specific* members.
That is not a limitation to be worked around; it is what makes it fast and
what makes its failure modes small enough to reason about. A volume that
exceeds a member is outside what it can express.

---

## Why two nodes are a special case

At two members, placement is degenerate. A two-replica volume on a two-node
cluster goes on both nodes; there is no decision to make, no rebalancing to
do, and no placement engine worth writing. Everything on the list above
except a bigger-than-a-node volume is reachable with the stack already
shipped, plus a capacity view and a drive-selection wizard.

This is why the two-node work is native and the larger-cluster work is not.
Not staging for its own sake — the second case genuinely needs machinery the
first has no use for.

### The 2-node native model (in progress)

- Each member contributes chosen disks to one local ZFS pool.
- The console sums the members' pools into one capacity figure, labelled
  **raw**, because a replicated volume consumes its full size on every member
  holding a replica. Usable capacity depends on per-volume replica count and
  is not a property of the hardware.
- A volume is created by naming a size; both members hold a replica, because
  at two members that is the only arrangement that survives losing one.
- `docs/cluster.md`'s two-node regime already handles the split-brain problem
  this creates: `two_node`, `wait_for_all`, and fencing through Pacemaker.

At three members this model still works and starts to strain: a 2-of-3 volume
requires the operator to choose *which* two, and nothing rebalances when a
fourth member arrives. That is the boundary.

---

## The choice at more than two nodes

### Option A — LINSTOR over the existing DRBD

LINSTOR is LINBIT's placement layer over DRBD: nodes contribute storage
pools, and a volume is created with a placement policy rather than a list of
members. It keeps the replication engine already shipped and adds exactly the
missing piece.

**What it gives:** policy-driven placement, aggregate capacity, automatic
replica selection, rebalancing on node add and remove, snapshot orchestration.

**What it does not give:** a volume larger than one member. Each replica is
still a whole copy on one node. Request 3 above remains unmet.

**Blocking problem: there is no EL10 package.** Verified against the sources
this appliance actually uses:

- ELRepo's `el10` tree carries `drbd9x-utils` and `kmod-drbd9x`. It carries
  no `linstor` and no `drbd-reactor`.
- The node's configured repositories — AlmaLinux BaseOS, AppStream, CRB,
  Extras, and the `lumen` repo — have no LINSTOR package.
- LINBIT distributes official binaries to support-contract customers. The
  source is GPL-3.0 and redistributable, but nobody publishes EL10 RPMs.

Shipping it therefore means packaging `linstor-server` (Java, built with
Gradle and protobuf), `linstor-client` (Python), and `drbd-reactor` (Rust,
for controller HA) into the `lumen` repo, **and putting a JRE into an
appliance image that currently has no Java at all**. That is a large, ongoing
maintenance commitment for a component that does not solve request 3.

### Option B — Ceph (RBD)

Ceph is the actual equivalent of what vSAN does. Objects are distributed
across the cluster by CRUSH, an RBD image is striped over many objects, and
the cluster self-heals by re-replicating under-protected placement groups
onto surviving OSDs.

**What it gives:** all three requests. Volumes larger than any member,
placement and rebalancing as a built-in, erasure coding as an alternative to
replication, and a single pool whose capacity is genuinely the sum of the
disks.

**What it costs:**

- **A different failure model.** DRBD's failure modes are per-volume and
  local. Ceph's are cluster-wide: a bad CRUSH change, a full OSD, or a
  network partition affects everything at once. The operational surface is
  much larger than the one this appliance presents today.
- **A floor of three nodes**, and realistically five before the defaults make
  sense. `size=3, min_size=2` is the configuration worth running; at three
  nodes, losing one leaves no room to re-replicate.
- **Dedicated fast networking.** Recovery traffic is heavy and competes with
  client I/O. The Core network already exists for replication and would carry
  this too, which raises its requirements considerably.
- **Daemons the appliance does not have.** MONs (odd number, quorum of their
  own — a second quorum system alongside corosync's), MGRs, OSDs per disk,
  and their placement and upgrade orchestration.
- **ZFS stops being the substrate.** Ceph OSDs want raw disks with BlueStore.
  A node cannot sensibly give the same disk to both a ZFS pool and an OSD, so
  the drive-selection wizard becomes a decision about which system owns each
  disk — and the boot pool stays ZFS regardless.

**Packaging: available, unlike LINSTOR.** The CentOS Storage SIG publishes
Ceph for EL10 — `ceph-squid` carries 19.2.0 through 19.2.5, with
`ceph-common`, `ceph-mon`, `ceph-mgr`, `ceph-osd`, and `cephadm` all present
at `mirror.stream.centos.org/SIGs/10-stream/storage/x86_64/`. Newer
`ceph-tentacle` and `ceph-umbrella` trees exist alongside it.

One caveat: the release tag is `el10s`, meaning CentOS Stream 10, while this
appliance is built on AlmaLinux 10. That is the same relationship ELRepo's
`el10` packages already have with the image and is expected to work, but it
is a mirroring decision for `iso/pins.env` rather than a package that simply
appears — and the Storage SIG is a community build, not an AlmaLinux one.
Version pinning would matter more here than it does for ZFS or DRBD, because
Ceph release trains move faster than either.

---

## Recommendation

Keep DRBD as the two-node story. It is shipped, it is understood, its failure
modes are small, and at two members it is not meaningfully worse than
anything that would replace it.

Adopt Ceph — not LINSTOR — for clusters of three or more, if and when that
size is a real target.

The reasoning is that LINSTOR's advantage is reusing DRBD, and that advantage
is largely cancelled by having to package a JVM service with no upstream EL10
build. Having paid a large integration cost, the result still cannot do the
one thing that a smaller cluster genuinely cannot: hold a volume bigger than
a node. If the cost has to be paid, it should buy the capability that is
otherwise unreachable.

The exception is a LINBIT subscription. With supported EL10 binaries, LINSTOR
becomes a modest integration on top of a replication engine already trusted
here, and the calculus reverses for any cluster that does not need
oversized volumes.

---

## If Ceph is chosen: the shape of the work

Rough sequencing, each phase independently useful and abandonable:

1. **Pin and mirror.** The packages exist (above); the work is adding the
   Storage SIG tree to `iso/pins.env` beside the ZFS and DRBD mirrors, at a
   pinned release, and confirming an `el10s` build installs cleanly on the
   AlmaLinux 10 image.
2. **A `lumen-ceph` crate**, alongside `lumen-drbd` and structured the same
   way — model, state, backend (`cli`/`mock`/`unavailable`), service. The
   backend shells to `ceph`, `rbd`, and `cephadm`. Mockable end to end so the
   workflows test without a cluster, exactly as the DRBD engine does.
3. **Disk ownership in the drive wizard.** Each disk is assigned to a ZFS
   pool or to an OSD, never both, with the boot pool excluded. This is where
   the two-node and larger-cluster models meet in the console.
4. **Bootstrap and daemon placement.** MONs on an odd subset, MGRs with
   failover, an OSD per assigned disk. The health of these belongs in the
   cluster view next to quorum and fencing, because it is a second quorum an
   operator must understand.
5. **RBD-backed machine disks**, alongside the DRBD path rather than
   replacing it. A cluster runs one or the other; both code paths exist while
   two-node clusters are still supported.
6. **A migration path** from DRBD volumes to RBD images, offline at first.

**What must not happen:** both systems active on one cluster. Two replication
engines with two quorum systems and two failure models, over the same disks
and the same network, is a system nobody can reason about during an incident.
The cluster's regime picks one.

---

## Open questions

- Is a three-node cluster a real target, or is two the product? If two is the
  product, none of this is needed and the native work is the whole story.
- Would a LINBIT subscription be considered? It changes the recommendation
  for every cluster that does not need oversized volumes.
- Erasure coding or replication for the default pool? EC is far more
  space-efficient and much worse for the small random writes virtual machine
  disks produce.
