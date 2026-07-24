#!/bin/bash
# SessionStart hook for Claude Code on the web.
#
# 1. Installs the build dependency web containers lack (libpam headers —
#    lumen-controlplane links libpam; without it every cargo build/test
#    fails).
# 2. Installs lumen-webui's npm dependencies so builds and type checks work
#    immediately.
# 3. Optionally configures git commit signing + identity from environment
#    secrets, so commits are authored and signed as the operator instead of
#    the harness's managed key (which GitHub shows as "Unverified").
#
#    Set these in the Claude Code environment settings (values are secrets;
#    nothing is committed to the repo):
#      LUMEN_GIT_USER_NAME        e.g.  Cody Wellman
#      LUMEN_GIT_USER_EMAIL       e.g.  cwellman@quartz.systems
#      LUMEN_GIT_SIGNING_KEY_B64  base64 of an OpenSSH ed25519 private key
#                                 whose public half is registered on the
#                                 author's GitHub account as a Signing Key
#    Sessions without the variables skip this step entirely.
set -euo pipefail

# Web sessions only — local checkouts manage their own toolchain and keys.
if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

# --- build dependencies -------------------------------------------------------
if ! dpkg -s libpam0g-dev >/dev/null 2>&1; then
  apt-get install -y libpam0g-dev >/dev/null 2>&1 \
    || { apt-get update >/dev/null 2>&1; apt-get install -y libpam0g-dev >/dev/null; }
fi
command -v ssh-keygen >/dev/null 2>&1 || apt-get install -y openssh-client >/dev/null

if [ -f "$CLAUDE_PROJECT_DIR/lumen-webui/package.json" ]; then
  (cd "$CLAUDE_PROJECT_DIR/lumen-webui" && npm install --no-audit --no-fund >/dev/null)
fi

# --- git identity + commit signing (optional, from environment secrets) -------
if [ -n "${LUMEN_GIT_SIGNING_KEY_B64:-}" ]; then
  key_file="$HOME/.ssh/lumen-signing"
  mkdir -p "$HOME/.ssh"
  umask 077
  printf '%s' "$LUMEN_GIT_SIGNING_KEY_B64" | base64 -d > "$key_file"
  # A key file without a trailing newline is rejected by ssh-keygen.
  [ -z "$(tail -c 1 "$key_file")" ] || echo >> "$key_file"
  ssh-keygen -y -f "$key_file" > "$key_file.pub"

  git -C "$CLAUDE_PROJECT_DIR" config gpg.format ssh
  git -C "$CLAUDE_PROJECT_DIR" config gpg.ssh.program ssh-keygen
  git -C "$CLAUDE_PROJECT_DIR" config user.signingkey "$key_file.pub"
  git -C "$CLAUDE_PROJECT_DIR" config commit.gpgsign true
  echo "session-start: commit signing configured (key $(ssh-keygen -lf "$key_file.pub" | cut -d' ' -f2))"
fi

if [ -n "${LUMEN_GIT_USER_NAME:-}" ] && [ -n "${LUMEN_GIT_USER_EMAIL:-}" ]; then
  git -C "$CLAUDE_PROJECT_DIR" config user.name "$LUMEN_GIT_USER_NAME"
  git -C "$CLAUDE_PROJECT_DIR" config user.email "$LUMEN_GIT_USER_EMAIL"
  echo "session-start: git identity set to $LUMEN_GIT_USER_NAME <$LUMEN_GIT_USER_EMAIL>"
fi
