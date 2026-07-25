"use client";

import { SectionIndex } from "@/components/SectionIndex";

/// Infrastructure is a section rather than a page now, so its index hops to
/// the first child the same way Networking's does — the static export cannot
/// redirect on the server.
export default function InfrastructureIndex() {
  return <SectionIndex to="/infrastructure/clusters" />;
}
