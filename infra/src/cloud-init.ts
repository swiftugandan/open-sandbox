import * as pulumi from "@pulumi/pulumi";

export function controllerUserData(args: {
  databaseUrl: pulumi.Output<string> | string;
  dbPassword: pulumi.Output<string>;
  grpcPort: number;
  proxyHttpPort: number;
  proxyGrpcPort: number;
  proxyInternalGrpcPort: number;
  volumeDevice: string;
  // Comp-9 fix: env passthrough for the auth tokens the comp-1/2 review
  // round added. Without these, the binaries refuse to start at all —
  // controller's management gRPC requires CONTROLLER_ADMIN_TOKEN, proxy
  // requires OPEN_SANDBOX_INTERNAL_TOKEN + TUNNEL_JOIN_TOKEN.
  controllerAdminToken: pulumi.Output<string>;
  internalToken: pulumi.Output<string>;
  tunnelJoinToken: pulumi.Output<string>;
  apiKey: pulumi.Output<string>;
  // Optional ACME settings for the proxy's public listener (comp-2 C5).
  // Unset = plaintext h2c (dev only).
  tunnelAcmeDomain?: string;
  acmeEmail?: string;
  // PLAN_12FACTOR.md Phase 2: optional overrides for the api gateway's
  // inter-service URLs. Single-host topology (the current default) uses
  // 127.0.0.1; a future multi-host deploy can point the api at a remote
  // controller/proxy without editing this file.
  apiControllerUrl?: string;
  apiProxyUrl?: string;
}): pulumi.Output<string> {
  const acmeBlock = args.tunnelAcmeDomain && args.acmeEmail
    ? `Environment=TUNNEL_ACME_DOMAIN=${args.tunnelAcmeDomain}
Environment=ACME_EMAIL=${args.acmeEmail}
Environment=ACME_CACHE_DIR=/mnt/data/acme-cache`
    : "";

  const apiControllerUrl = args.apiControllerUrl ?? `http://127.0.0.1:${args.grpcPort}`;
  const apiProxyUrl = args.apiProxyUrl ?? `http://127.0.0.1:${args.proxyInternalGrpcPort}`;

  return pulumi.interpolate`#!/bin/bash
set -euo pipefail

# ── Mount block volume for Postgres data ──────────────────────
DEVICE="${args.volumeDevice}"
MOUNT="/mnt/data"
mkdir -p "$MOUNT"
if ! blkid "$DEVICE"; then
  mkfs.ext4 "$DEVICE"
fi
mount "$DEVICE" "$MOUNT"
echo "$DEVICE $MOUNT ext4 defaults,nofail 0 2" >> /etc/fstab

# ── Install postgresql ────────────────────────────────────────
apt-get update -qq
apt-get install -y postgresql postgresql-contrib

systemctl stop postgresql
PG_DATA="$MOUNT/postgresql"
if [ ! -d "$PG_DATA" ]; then
  mkdir -p "$PG_DATA"
  chown postgres:postgres "$PG_DATA"
  su - postgres -c "/usr/lib/postgresql/*/bin/initdb -D $PG_DATA"
fi

cat > /etc/postgresql-custom.conf <<PGCONF
data_directory = '$PG_DATA'
listen_addresses = '127.0.0.1'
port = 5432
PGCONF

systemctl start postgresql
su - postgres -c "psql -c \\"CREATE DATABASE open_sandbox;\\" || true"
# Comp-9: set a non-trivial postgres password so any future RCE in the
# controller doesn't become an instant DB superuser pwn. The binaries
# connect using this password baked into DATABASE_URL.
su - postgres -c "psql -c \\"ALTER USER postgres PASSWORD '${args.dbPassword}';\\""
# Switch pg_hba.conf from trust to md5 for local TCP connections so the
# password is actually enforced.
PG_HBA=$(su - postgres -c "psql -tA -c 'SHOW hba_file;'")
sed -i 's/^host\\s\\+all\\s\\+all\\s\\+127.0.0.1\\/32\\s\\+trust/host all all 127.0.0.1\\/32 md5/' "$PG_HBA"
sed -i 's/^host\\s\\+all\\s\\+all\\s\\+::1\\/128\\s\\+trust/host all all ::1\\/128 md5/' "$PG_HBA"
systemctl reload postgresql

# ── Install open-sandbox binary ───────────────────────────────
# Comp-9: pick the right arch for the host. Hetzner cax11 is aarch64;
# cx22 is amd64. Without this detection the cax11 default silently
# failed with "Exec format error" on the systemd unit. Pinned to the
# latest tag; for production set OPEN_SANDBOX_VERSION env var to a
# specific release.
case "$(uname -m)" in
  aarch64|arm64) BIN_ARCH=arm64 ;;
  x86_64|amd64)  BIN_ARCH=amd64 ;;
  *) echo "unsupported architecture: $(uname -m)"; exit 1 ;;
esac
BINARY_NAME="open-sandbox-linux-\${BIN_ARCH}"
RELEASE_BASE="https://github.com/chamuka-inc/open-sandbox/releases/latest/download"
curl -fsSL "\${RELEASE_BASE}/\${BINARY_NAME}" -o /usr/local/bin/open-sandbox
chmod +x /usr/local/bin/open-sandbox

# PLAN_12FACTOR.md Phase 5 / factor #5: verify the binary against
# SHA256SUMS published alongside the release. Handles GNU coreutils
# text format ("hash  name"), GNU binary format ("hash *name"), and
# BSD --tag format ("SHA256 (name) = hash"), and strips CRLF.
#
# Decision matrix:
#   SHA256SUMS missing       → warn + continue (back-compat).
#   SHA256SUMS present but
#     no entry for our binary → FATAL (likely tampered or misconfigured
#                                publisher; safer to bail than to silently
#                                install an unverifiable binary).
#   Entry present, hash match → proceed.
#   Entry present, mismatch  → FATAL.
if curl -fsSL "\${RELEASE_BASE}/SHA256SUMS" -o /tmp/SHA256SUMS 2>/dev/null; then
  EXPECTED=$(awk -v name="\${BINARY_NAME}" '
    # GNU text or binary format: hash in $1, name (possibly *-prefixed) in $2
    $2 == name || $2 == "*" name { print $1; exit }
    # BSD --tag format: "SHA256 (name) = hash"
    $1 == "SHA256" && $2 == "(" name ")" { print $4; exit }
  ' /tmp/SHA256SUMS | tr -d "\\r")
  if [ -z "\${EXPECTED}" ]; then
    echo "FATAL: SHA256SUMS fetched from \${RELEASE_BASE} but contains no entry for \${BINARY_NAME}" >&2
    echo "  This may indicate the release was tampered with, the publisher's pipeline is misconfigured," >&2
    echo "  or the SHA256SUMS file uses an unrecognized format. Refusing to install an unverifiable binary." >&2
    rm -f /usr/local/bin/open-sandbox /tmp/SHA256SUMS
    exit 1
  fi
  ACTUAL=$(sha256sum /usr/local/bin/open-sandbox | awk '{print $1}')
  if [ "\${EXPECTED}" != "\${ACTUAL}" ]; then
    echo "FATAL: SHA256 mismatch for \${BINARY_NAME}" >&2
    echo "  expected: \${EXPECTED}" >&2
    echo "  actual:   \${ACTUAL}" >&2
    rm -f /usr/local/bin/open-sandbox /tmp/SHA256SUMS
    exit 1
  fi
  echo "verified open-sandbox binary against published SHA256SUMS"
  rm -f /tmp/SHA256SUMS
else
  echo "warning: no SHA256SUMS at \${RELEASE_BASE}/SHA256SUMS; binary integrity NOT verified" >&2
  echo "  publish SHA256SUMS alongside release binaries to enable verification" >&2
fi

# ── Run schema migrations once before starting services ──────
# PLAN_12FACTOR.md Phase 3 / factor #12: migrations are an admin
# process, not part of service startup. Running them once here means
# a migration failure surfaces at cloud-init time (visible in the
# server's cloud-init log) instead of cascading into a systemd
# unit-start failure that crash-loops. The command is idempotent
# (CREATE TABLE/INDEX IF NOT EXISTS), so re-runs on volume reuse
# are safe.
#
# The URL is routed through a quoted heredoc (\`<<'DBURL_EOF'\`)
# before being assigned, so bash performs ZERO expansion on the
# content — a dbPassword containing \`$\`, backtick, \`\\\`, or \`"\`
# would otherwise be corrupted by the shell before reaching the
# binary. systemd \`Environment=\` lines below have the same property
# natively; this heredoc gives us the same guarantee in shell.
read -r DBURL_VAR <<'DBURL_EOF'
${args.databaseUrl}
DBURL_EOF
export OPEN_SANDBOX_DATABASE_URL="$DBURL_VAR"
/usr/local/bin/open-sandbox migrate
unset OPEN_SANDBOX_DATABASE_URL DBURL_VAR

# ── Systemd: open-sandbox controller ─────────────────────────
# Code-review finding #5: ExecStartPre runs migrate on every (re)start
# so an in-place binary upgrade (replacing /usr/local/bin/open-sandbox
# and \`systemctl restart\`) re-applies schema migrations rather than
# silently running a new binary against an old schema. The inline
# migrate call above handles fresh-provisioning visibility; this
# handles the upgrade path. Both are idempotent (CREATE ... IF NOT
# EXISTS).
cat > /etc/systemd/system/open-sandbox-controller.service <<UNIT
[Unit]
Description=open-sandbox controller
After=postgresql.service
Requires=postgresql.service

[Service]
ExecStartPre=/usr/local/bin/open-sandbox migrate
ExecStart=/usr/local/bin/open-sandbox controller --grpc-port ${args.grpcPort}
Environment=OPEN_SANDBOX_DATABASE_URL=${args.databaseUrl}
Environment=CONTROLLER_ADMIN_TOKEN=${args.controllerAdminToken}
Environment=OPEN_SANDBOX_JOIN_TOKEN=${args.tunnelJoinToken}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

# ── Systemd: open-sandbox proxy ──────────────────────────────
# Comp-9: TUNNEL_JOIN_TOKEN (agents present this on OpenTunnel),
# OPEN_SANDBOX_INTERNAL_TOKEN (api gateway → proxy OpenIoStream auth),
# and the optional ACME settings to terminate TLS on the public listener.
cat > /etc/systemd/system/open-sandbox-proxy.service <<UNIT
[Unit]
Description=open-sandbox proxy
After=postgresql.service
Requires=postgresql.service

[Service]
ExecStart=/usr/local/bin/open-sandbox proxy --http-port ${args.proxyHttpPort} --grpc-port ${args.proxyGrpcPort} --internal-grpc-port ${args.proxyInternalGrpcPort}
Environment=OPEN_SANDBOX_DATABASE_URL=${args.databaseUrl}
Environment=OPEN_SANDBOX_INTERNAL_TOKEN=${args.internalToken}
Environment=TUNNEL_JOIN_TOKEN=${args.tunnelJoinToken}
${acmeBlock}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

# ── Systemd: open-sandbox api gateway ────────────────────────
# Co-located with controller + proxy on the same VM by default.
# OPEN_SANDBOX_INTERNAL_TOKEN must match the proxy's setting so
# OpenIoStream calls authenticate. The controller/proxy URLs default
# to 127.0.0.1 (single-host topology) but can be overridden by the
# Pulumi caller via apiControllerUrl / apiProxyUrl for multi-host
# deploys (see PLAN_12FACTOR.md Phase 2).
cat > /etc/systemd/system/open-sandbox-api.service <<UNIT
[Unit]
Description=open-sandbox api gateway
After=open-sandbox-controller.service open-sandbox-proxy.service
Requires=open-sandbox-controller.service open-sandbox-proxy.service

[Service]
ExecStart=/usr/local/bin/open-sandbox api
Environment=OPEN_SANDBOX_CONTROLLER_URL=${apiControllerUrl}
Environment=OPEN_SANDBOX_PROXY_URL=${apiProxyUrl}
Environment=OPEN_SANDBOX_API_KEY=${args.apiKey}
Environment=OPEN_SANDBOX_INTERNAL_TOKEN=${args.internalToken}
Environment=CONTROLLER_ADMIN_TOKEN=${args.controllerAdminToken}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable --now open-sandbox-controller
systemctl enable --now open-sandbox-proxy
systemctl enable --now open-sandbox-api

# ── pg_dump backup cron (6-hour RPO) ─────────────────────────
cat > /etc/cron.d/pg-backup <<CRON
0 */6 * * * postgres pg_dump -Fc open_sandbox > /mnt/data/backups/open_sandbox_\\$(date +\\%Y\\%m\\%d_\\%H\\%M).dump
CRON
mkdir -p /mnt/data/backups
chown postgres:postgres /mnt/data/backups
`;
}

export function workerUserData(args: {
  controllerUrl: string;
  proxyUrl: string;
  joinToken: pulumi.Output<string> | string;
  tunnelJoinToken: pulumi.Output<string>;
}): pulumi.Output<string> {
  // Comp-9: workers need TUNNEL_JOIN_TOKEN so they can present it when
  // dialing the proxy's OpenTunnel listener (comp-2 A1). Without it the
  // agent binary refuses to start.
  return pulumi.interpolate`#!/bin/bash
set -euo pipefail

# ── Install Docker ────────────────────────────────────────────
apt-get update -qq
apt-get install -y docker.io
systemctl enable --now docker

# ── Install open-sandbox binary ───────────────────────────────
# NOTE: worker host arch is hardcoded amd64 here; the controller's
# cloud-init detects arch via \`uname -m\`. Workers have not yet been
# tested on aarch64 hosts.
BINARY_NAME="open-sandbox-linux-amd64"
RELEASE_BASE="https://github.com/chamuka-inc/open-sandbox/releases/latest/download"
curl -fsSL "\${RELEASE_BASE}/\${BINARY_NAME}" -o /usr/local/bin/open-sandbox
chmod +x /usr/local/bin/open-sandbox

# PLAN_12FACTOR.md Phase 5 / factor #5: verify against SHA256SUMS.
# Handles GNU text, GNU binary, and BSD --tag formats; strips CRLF.
# Missing SUMS file → warn + continue (back-compat); missing entry
# for our binary → FATAL (refuse to install an unverifiable binary).
if curl -fsSL "\${RELEASE_BASE}/SHA256SUMS" -o /tmp/SHA256SUMS 2>/dev/null; then
  EXPECTED=$(awk -v name="\${BINARY_NAME}" '
    $2 == name || $2 == "*" name { print $1; exit }
    $1 == "SHA256" && $2 == "(" name ")" { print $4; exit }
  ' /tmp/SHA256SUMS | tr -d "\\r")
  if [ -z "\${EXPECTED}" ]; then
    echo "FATAL: SHA256SUMS fetched but contains no entry for \${BINARY_NAME}" >&2
    rm -f /usr/local/bin/open-sandbox /tmp/SHA256SUMS
    exit 1
  fi
  ACTUAL=$(sha256sum /usr/local/bin/open-sandbox | awk '{print $1}')
  if [ "\${EXPECTED}" != "\${ACTUAL}" ]; then
    echo "FATAL: SHA256 mismatch for \${BINARY_NAME}" >&2
    echo "  expected: \${EXPECTED}" >&2
    echo "  actual:   \${ACTUAL}" >&2
    rm -f /usr/local/bin/open-sandbox /tmp/SHA256SUMS
    exit 1
  fi
  echo "verified open-sandbox binary against published SHA256SUMS"
  rm -f /tmp/SHA256SUMS
else
  echo "warning: no SHA256SUMS at \${RELEASE_BASE}/SHA256SUMS; binary integrity NOT verified" >&2
fi

# ── Systemd: open-sandbox agent ──────────────────────────────
cat > /etc/systemd/system/open-sandbox-agent.service <<UNIT
[Unit]
Description=open-sandbox agent
After=docker.service
Requires=docker.service

[Service]
ExecStart=/usr/local/bin/open-sandbox agent --controller-url ${args.controllerUrl} --proxy-url ${args.proxyUrl}
Environment=OPEN_SANDBOX_JOIN_TOKEN=${args.joinToken}
Environment=TUNNEL_JOIN_TOKEN=${args.tunnelJoinToken}
Environment=OPEN_SANDBOX_CONTROLLER_URL=${args.controllerUrl}
Environment=OPEN_SANDBOX_PROXY_URL=${args.proxyUrl}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable --now open-sandbox-agent
`;
}
