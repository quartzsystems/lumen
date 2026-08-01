"use client";

import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { PooledStorageSection } from "@/components/storage/PooledStorage";

/// Pooled storage: the one storage object a cluster owns rather than a node.
///
/// Its own page rather than a section under the pool table, because it is a
/// different scope. A pool belongs to the node it is on; the cluster's pool
/// belongs to the cluster, exists on several members at once, and serves
/// every machine disk on all of them. Reading it at the bottom of a per-node
/// table made it look like one more thing that node has.
///
/// The section renders nothing at all on a node without the feature —
/// absent rather than empty, which is the honest shape: pooling is a
/// cluster feature and a standalone appliance has no half of it to show.
export default function VolumesPage() {
  return (
    <Page>
      <PageHeader
        title="Volumes"
        description="Cluster-owned storage: one disk on several members at once, and which member is serving it."
      />
      <PageBody>
        <PooledStorageSection />
      </PageBody>
    </Page>
  );
}
