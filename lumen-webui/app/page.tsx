"use client";

import { SectionIndex } from "@/components/SectionIndex";

/// Site root. The console proper lives under /dashboard (inside the authed
/// shell), so the root just forwards there; AuthGuard bounces visitors without
/// a session on to /login.
export default function RootIndex() {
  return <SectionIndex to="/dashboard" />;
}
