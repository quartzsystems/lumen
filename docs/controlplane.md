# Lumen control plane & web UI

Two top-level components deliver the hypervisor's management surface:

```
lumen-controlplane/   Rust (axum) daemon: auth API + serves the web UI on :8443
lumen-webui/          Next.js 16 + TypeScript + Tailwind CSS 4 console
```

## Architecture

One daemon, one port. In production `lumen-controlplane` listens on
**https://\<host\>:8443** and serves both the REST API (`/api/...`) and the
static web UI export — no Node.js on the appliance. TLS is a self-signed
certificate minted into the state dir on first boot (replaceable via
`LUMEN_CP_TLS_CERT`/`LUMEN_CP_TLS_KEY`), like every hypervisor web UI.

### Authentication: realms

Sign-in goes through a **realm** — a pluggable credential verifier
(`src/realm/`). The stock realm is **`lumen`**: the auth built into the OS
itself. It validates credentials through PAM (service
`lumen-controlplane`, shipped in `lumen-controlplane/pam/`, delegating to
the EL `system-auth` stack), so shadow policy, faillock, and account
expiry apply to web sign-ins exactly as they do on the console. Today the
appliance has a single account — root, with the password chosen in the
installer. Additional realms (LDAP, OIDC, …) implement the same `Realm`
trait and appear automatically in the login page's Realm dropdown
(`GET /api/auth/realms`).

The PAM layer is a deliberate ~150-line in-tree FFI
(`src/realm/pam_ffi.rs`): the published pam crates all run bindgen at
build time, which would drag libclang into the appliance/CI toolchains.
Building only needs `pam-devel` (EL) / `libpam0g-dev` (Debian).

### Sessions

A successful login issues an HS256 JWT **session ticket** in an
`httpOnly` `Secure` `SameSite=Lax` cookie (`lumen_auth`, 12 h TTL) — the
same model as Quartz Command. The signing secret is minted on first boot
into the state dir; deleting `session-secret` invalidates every
outstanding session. JS never sees the ticket; the client only caches a
non-sensitive display user in localStorage.

### API

| Endpoint               | Method | Purpose                                    |
| ---------------------- | ------ | ------------------------------------------ |
| `/api/auth/realms`     | GET    | Realms for the login dropdown (public)     |
| `/api/auth/login`      | POST   | `{username, password, realm}` → ticket     |
| `/api/auth/logout`     | POST   | Clear the session cookie                   |
| `/api/auth/me`         | GET    | Current principal, or 401                  |
| `/api/version`         | GET    | Lumen version (from the VERSION file)      |

Errors are `{ "error": "<user-facing text>" }`. Login failures are a
uniform 401 regardless of cause, so responses don't leak whether an
account exists.

### Configuration (environment)

| Variable                   | Default                       |
| -------------------------- | ----------------------------- |
| `LUMEN_CP_LISTEN`          | `0.0.0.0:8443`                |
| `LUMEN_CP_STATE_DIR`       | `/var/lib/lumen-controlplane` |
| `LUMEN_CP_WEBUI_DIR`       | `/usr/share/lumen-webui`      |
| `LUMEN_CP_TLS_CERT`/`_KEY` | unset (self-signed minted)    |
| `LUMEN_CP_PAM_SERVICE`     | `lumen-controlplane`          |
| `LUMEN_CP_SESSION_TTL_SECS`| `43200` (12 h)                |
| `LUMEN_CP_NO_TLS`          | unset (dev only: `1` = HTTP)  |

## Web UI

`lumen-webui` is the Quartz design system (tokens shared with Quartz
Command, dark-first, Manrope vendored from `branding/fonts` so builds
never touch the network). The login page is Quartz Command's sign-in card
with the Lumen mark, a **Username** field (the built-in realm
authenticates OS accounts, not emails), and a **Realm** dropdown fed by
`/api/auth/realms`.

Production builds are a static export (`npm run build` → `out/`,
`trailingSlash` so routes are directories) that the controlplane serves
itself.

## Development

```sh
# Terminal 1 — API on plain HTTP :8443 (PAM needs an /etc/pam.d/lumen-controlplane;
# copy lumen-controlplane/pam/lumen-controlplane, adapting to common-auth on Debian/Ubuntu)
cd lumen-controlplane && LUMEN_CP_NO_TLS=1 cargo run

# Terminal 2 — UI with hot reload on :3000; /api proxies to the controlplane
cd lumen-webui && npm install && npm run dev
```

The browser only ever talks to one origin in both dev and production, so
the session cookie stays first-party.

To try the production shape locally:

```sh
make controlplane webui
LUMEN_CP_WEBUI_DIR=lumen-webui/out ./build/cargo-target-cp/release/lumen-controlplane
```

## Validation

`make test` runs the controlplane's unit + API tests (a mock realm stands
in for PAM, so no OS accounts are touched); `make lint` adds
fmt/clippy. CI builds both components on every push (`controlplane` and
`webui` jobs in `.github/workflows/ci.yml`).

## Appliance integration

`make rpms` packages the daemon, the web UI export, the PAM service, the
systemd unit, and a firewalld service definition into the
**lumen-controlplane** RPM (see `packages/lumen-controlplane.spec`; the
binary and export are compiled by `packages/build-rpms.sh` before
rpmbuild, since cargo/npm need the network). The ISO pipeline ships the
RPM in the on-media `lumen` repo and the offline-resolve gate covers it;
the installer adds it to the target package set, enables
`lumen-controlplane.service`, and opens 8443 via
`firewall-offline-cmd --add-service=lumen-controlplane`. First boot of an
installed appliance therefore serves the console at
`https://<management-ip>:8443` with no manual steps.
