"use client";

import { useEffect } from "react";
import { useRouter } from "next/navigation";

/// Section index route. The production build is a static export, which can't
/// server-redirect, so a section root hops to its first child client-side.
export function SectionIndex({ to }: { to: string }) {
  const router = useRouter();
  useEffect(() => {
    router.replace(to);
  }, [router, to]);
  return null;
}
