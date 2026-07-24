import { apiFetch } from "@/lib/authClient";

export interface VersionInfo {
  version: string;
}

/// GET /api/version — the control plane's build version.
export const fetchVersion = (): Promise<VersionInfo> => apiFetch<VersionInfo>("/version");
