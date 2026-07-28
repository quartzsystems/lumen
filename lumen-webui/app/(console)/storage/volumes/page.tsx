"use client";

import { Page, PageBody, PageHeader } from "@/components/PageHeader";
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
/// The section renders nothing at all on a node with no clusters — absent
/// rather than empty, which is the honest shape: replication is a cluster
/// feature and a standalone appliance has no half of it to show.
export default function VolumesPage() {
  return (
    <Page>
      <PageHeader
        title="Volumes"
        description="Replicated volumes: one disk on several members at once, and which replica is where."
      />
      <PageBody>
        <ReplicatedVolumesSection />
      </PageBody>
    </Page>
  );
}
