import * as pulumi from "@pulumi/pulumi";

export function controllerUserData(args: {
  databaseUrl: string;
  grpcPort: number;
  proxyHttpPort: number;
  proxyGrpcPort: number;
  volumeDevice: string;
}): string {
  return `#!/bin/bash
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

# ── Install open-sandbox binary ───────────────────────────────
curl -fsSL https://github.com/chamuka-inc/open-sandbox/releases/latest/download/open-sandbox-linux-amd64 \\
  -o /usr/local/bin/open-sandbox
chmod +x /usr/local/bin/open-sandbox

# ── Systemd: open-sandbox controller ─────────────────────────
cat > /etc/systemd/system/open-sandbox-controller.service <<UNIT
[Unit]
Description=open-sandbox controller
After=postgresql.service
Requires=postgresql.service

[Service]
ExecStart=/usr/local/bin/open-sandbox controller --grpc-port ${args.grpcPort}
Environment=OPEN_SANDBOX_DATABASE_URL=${args.databaseUrl}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

# ── Systemd: open-sandbox proxy ──────────────────────────────
cat > /etc/systemd/system/open-sandbox-proxy.service <<UNIT
[Unit]
Description=open-sandbox proxy
After=postgresql.service
Requires=postgresql.service

[Service]
ExecStart=/usr/local/bin/open-sandbox proxy --http-port ${args.proxyHttpPort} --grpc-port ${args.proxyGrpcPort}
Environment=OPEN_SANDBOX_DATABASE_URL=${args.databaseUrl}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable --now open-sandbox-controller
systemctl enable --now open-sandbox-proxy

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
}): pulumi.Output<string> {
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
