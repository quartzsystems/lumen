import type { NextConfig } from "next";

// Production builds are a fully static export (`out/`) that lumen-controlplane
// serves itself on :8443 — one daemon, no Node.js on the appliance. Routes are
// exported as directories (login/index.html) so the controlplane's static
// file server resolves /login without knowing Next's flat-html convention.
//
// `next dev` can't combine with `output: "export"` rewrites, so dev mode drops
// the export and proxies /api to a locally running controlplane instead
// (LUMEN_CP_NO_TLS=1 cargo run; override the target with LUMEN_API_URL).
// The browser only ever talks to the Next origin, keeping the httpOnly
// session cookie first-party in development exactly as it is in production.
const API_URL = process.env.LUMEN_API_URL || "http://127.0.0.1:8443";

const nextConfig: NextConfig = {
  trailingSlash: true,
  ...(process.env.NODE_ENV === "production"
    ? { output: "export" as const }
    : {
        async rewrites() {
          return [{ source: "/api/:path*", destination: `${API_URL}/api/:path*` }];
        },
      }),
};

export default nextConfig;
