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
  // requires INTERNAL_TOKEN + TUNNEL_JOIN_TOKEN.
  controllerAdminToken: pulumi.Output<string>;
  internalToken: pulumi.Output<string>;
  tunnelJoinToken: pulumi.Output<string>;
  apiKey: pulumi.Output<string>;
  // Optional ACME settings for the proxy's public listener (comp-2 C5).
  // Unset = plaintext h2c (dev only).
  tunnelAcmeDomain?: string;
  acmeEmail?: string;
}): pulumi.Output<string> {
  const acmeBlock = args.tunnelAcmeDomain && args.acmeEmail
    ? `Environment=TUNNEL_ACME_DOMAIN=${args.tunnelAcmeDomain}
Environment=ACME_EMAIL=${args.acmeEmail}
Environment=ACME_CACHE_DIR=/mnt/data/acme-cache`
    : "";

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
# specific release and add a checksum verification step.
case "$(uname -m)" in
  aarch64|arm64) BIN_ARCH=arm64 ;;
  x86_64|amd64)  BIN_ARCH=amd64 ;;
  *) echo "unsupported architecture: $(uname -m)"; exit 1 ;;
esac
BINARY_URL="https://github.com/chamuka-inc/open-sandbox/releases/latest/download/open-sandbox-linux-\${BIN_ARCH}"
curl -fsSL "$BINARY_URL" -o /usr/local/bin/open-sandbox
chmod +x /usr/local/bin/open-sandbox
# Optional: verify checksum if OPEN_SANDBOX_SHA256 is set in the env
# (cloud-init doesn't pass env, but operators using pulumi.interpolate
# to bake the sha256 into the script can drop a verification step in
# here).

# ── Systemd: open-sandbox controller ─────────────────────────
cat > /etc/systemd/system/open-sandbox-controller.service <<UNIT
[Unit]
Description=open-sandbox controller
After=postgresql.service
Requires=postgresql.service

[Service]
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
# INTERNAL_TOKEN (api gateway → proxy OpenIoStream auth), and the
# optional ACME settings to terminate TLS on the public listener.
cat > /etc/systemd/system/open-sandbox-proxy.service <<UNIT
[Unit]
Description=open-sandbox proxy
After=postgresql.service
Requires=postgresql.service

[Service]
ExecStart=/usr/local/bin/open-sandbox proxy --http-port ${args.proxyHttpPort} --grpc-port ${args.proxyGrpcPort} --internal-grpc-port ${args.proxyInternalGrpcPort}
Environment=OPEN_SANDBOX_DATABASE_URL=${args.databaseUrl}
Environment=INTERNAL_TOKEN=${args.internalToken}
Environment=TUNNEL_JOIN_TOKEN=${args.tunnelJoinToken}
${acmeBlock}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

# ── Systemd: open-sandbox api gateway ────────────────────────
# Co-located with controller + proxy on the same VM. INTERNAL_TOKEN
# must match the proxy's setting so OpenIoStream calls authenticate.
cat > /etc/systemd/system/open-sandbox-api.service <<UNIT
[Unit]
Description=open-sandbox api gateway
After=open-sandbox-controller.service open-sandbox-proxy.service
Requires=open-sandbox-controller.service open-sandbox-proxy.service

[Service]
ExecStart=/usr/local/bin/open-sandbox api
Environment=OPEN_SANDBOX_CONTROLLER_URL=http://127.0.0.1:${args.grpcPort}
Environment=OPEN_SANDBOX_PROXY_URL=http://127.0.0.1:${args.proxyInternalGrpcPort}
Environment=OPEN_SANDBOX_API_KEY=${args.apiKey}
Environment=INTERNAL_TOKEN=${args.internalToken}
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
curl -fsSL https://github.com/chamuka-inc/open-sandbox/releases/latest/download/open-sandbox-linux-amd64 \
  -o /usr/local/bin/open-sandbox
chmod +x /usr/local/bin/open-sandbox

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
