"use client";

import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { PooledStorageSection } from "@/components/storage/PooledStorage";
import { ReplicatedVolumesSection } from "@/components/storage/ReplicatedVolumes";

/// Replicated volumes: the one storage object a cluster owns rather than a
/// node.
///
/// Its own page rather than a section under the pool table, because it is a
/// different scope. A pool belongs to the node it is on; a replicated volume
/// belongs to the cluster, exists on several members at once, and fails over
/// with the machine that uses it. Reading it at the bottom of a per-node table
/// made it look like one more thing that node has.
///
/// Each section renders nothing at all on a node without its feature —
/// absent rather than empty, which is the honest shape: replication and
/// pooling are cluster features and a standalone appliance has no half of
/// either to show. A cluster runs one engine or the other, so at most one
/// section appears.
export default function VolumesPage() {
  return (
    <Page>
      <PageHeader
        title="Volumes"
        description="Cluster-owned storage: one disk on several members at once, and which member is serving it."
      />
      <PageBody>
        <PooledStorageSection />
        <ReplicatedVolumesSection />
      </PageBody>
    </Page>
  );
}
