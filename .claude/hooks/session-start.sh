#!/bin/bash
# SessionStart hook for Claude Code on the web.
#
# 1. Configures the operator's git identity (and commit signing, when the key
#    and tooling are both available) so commits are authored as the operator
#    rather than the harness's managed "Claude <noreply@anthropic.com>".
# 2. Installs the build dependency web containers lack (libpam headers —
#    lumen-controlplane links libpam; without it every cargo build/test fails).
# 3. Installs lumen-webui's npm dependencies so builds and type checks work
#    immediately.
#
#    Set these in the Claude Code environment settings (values are secrets;
#    nothing is committed to the repo):
#      LUMEN_GIT_USER_NAME        e.g.  Cody Wellman
#      LUMEN_GIT_USER_EMAIL       e.g.  cwellman@quartz.systems
#      LUMEN_GIT_SIGNING_KEY_B64  base64 of an OpenSSH ed25519 private key
#                                 whose public half is registered on the
#                                 author's GitHub account as a Signing Key
#    Sessions without the variables skip the step that needs them.
#
# Ordering and error handling matter here: identity is set FIRST, because it is
# free and cannot fail, and every step afterwards is best-effort. An earlier
# revision ran the package installs first under `set -e` and aborted on a failed
# `apt-get install openssh-client` (stale package index), which silently skipped
# the identity config for the whole session. Nothing below may be fatal.
set -uo pipefail

# Web sessions only — local checkouts manage their own toolchain and keys.
if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

# The harness normally exports CLAUDE_PROJECT_DIR; fall back to this script's
# own location so the hook still works if it ever isn't set.
PROJECT_DIR="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"

# --- git identity (first: cheap, and everything else is optional) -------------
if [ -n "${LUMEN_GIT_USER_NAME:-}" ] && [ -n "${LUMEN_GIT_USER_EMAIL:-}" ]; then
  git -C "$PROJECT_DIR" config user.name "$LUMEN_GIT_USER_NAME"
  git -C "$PROJECT_DIR" config user.email "$LUMEN_GIT_USER_EMAIL"
  echo "session-start: git identity set to $LUMEN_GIT_USER_NAME <$LUMEN_GIT_USER_EMAIL>"
else
  echo "session-start: LUMEN_GIT_USER_NAME/EMAIL unset — commits will use the harness identity" >&2
fi

# --- commit signing (needs both the key and ssh-keygen) -----------------------
# The harness signs with its own managed key by default. Signing someone else's
# name with that key is worse than not signing — GitHub renders it "Unverified"
# — so when we have set the operator's identity but cannot sign as them, turn
# signing off for this repo instead of leaving the mismatch in place.
signing_configured=false
if [ -n "${LUMEN_GIT_SIGNING_KEY_B64:-}" ] && ! command -v ssh-keygen >/dev/null 2>&1; then
  # Web containers ship without openssh-client, and their package index is old
  # enough that a bare `apt-get install` 404s on the pinned .deb — refresh
  # first. Best-effort: no ssh-keygen just means unsigned commits.
  apt-get update >/dev/null 2>&1 && apt-get install -y openssh-client >/dev/null 2>&1
fi

if [ -n "${LUMEN_GIT_SIGNING_KEY_B64:-}" ] && command -v ssh-keygen >/dev/null 2>&1; then
  key_file="$HOME/.ssh/lumen-signing"
  if mkdir -p "$HOME/.ssh" \
     && (umask 077; printf '%s' "$LUMEN_GIT_SIGNING_KEY_B64" | base64 -d > "$key_file") \
     && [ -s "$key_file" ]; then
    # A key file without a trailing newline is rejected by ssh-keygen.
    [ -z "$(tail -c 1 "$key_file")" ] || echo >> "$key_file"
    if ssh-keygen -y -f "$key_file" > "$key_file.pub" 2>/dev/null; then
      git -C "$PROJECT_DIR" config gpg.format ssh
      git -C "$PROJECT_DIR" config gpg.ssh.program ssh-keygen
      git -C "$PROJECT_DIR" config user.signingkey "$key_file.pub"
      git -C "$PROJECT_DIR" config commit.gpgsign true
      signing_configured=true
      echo "session-start: commit signing configured (key $(ssh-keygen -lf "$key_file.pub" | cut -d' ' -f2))"
    fi
  fi
  [ "$signing_configured" = true ] \
    || echo "session-start: signing key unusable — continuing unsigned" >&2
elif [ -n "${LUMEN_GIT_SIGNING_KEY_B64:-}" ]; then
  echo "session-start: ssh-keygen not installed — continuing unsigned" >&2
fi

if [ "$signing_configured" != true ] && [ -n "${LUMEN_GIT_USER_NAME:-}" ]; then
  git -C "$PROJECT_DIR" config commit.gpgsign false
fi

# --- build dependencies (best-effort; a failure must not abort the hook) ------
if ! dpkg -s libpam0g-dev >/dev/null 2>&1; then
  apt-get install -y libpam0g-dev >/dev/null 2>&1 \
    || { apt-get update >/dev/null 2>&1; apt-get install -y libpam0g-dev >/dev/null 2>&1; } \
    || echo "session-start: could not install libpam0g-dev — cargo builds of lumen-controlplane will fail" >&2
fi

if [ -f "$PROJECT_DIR/lumen-webui/package.json" ]; then
  (cd "$PROJECT_DIR/lumen-webui" && npm install --no-audit --no-fund >/dev/null 2>&1) \
    || echo "session-start: npm install failed — run it manually in lumen-webui" >&2
fi

exit 0
