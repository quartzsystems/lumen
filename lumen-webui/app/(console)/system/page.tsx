"use client";

import { SectionIndex } from "@/components/SectionIndex";

/// System is a section rather than a page, so its index hops to the first
/// child the same way Networking's does — the static export cannot redirect on
/// the server.
export default function SystemIndex() {
  return <SectionIndex to="/system/general" />;
}
