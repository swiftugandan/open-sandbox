export const CONTROLLER_GRPC_PORT = 50051;
export const PROXY_HTTP_PORT = 443;
// Comp-2 two-listener split: agents reach the public port (50052), the
// api gateway reaches the internal port (50053). Setting both to the
// same value collapses to a single combined listener (dev only).
export const PROXY_GRPC_PORT = 50052;
export const PROXY_INTERNAL_GRPC_PORT = 50053;
export const POSTGRES_PORT = 5432;
export const CONTROLLER_PRIVATE_IP = "10.0.0.2";
export const DATABASE_URL = `postgres://postgres@127.0.0.1:${POSTGRES_PORT}/open_sandbox`;
export const UBUNTU_IMAGE = "ubuntu-24.04";
export const VOLUME_DEVICE_PREFIX = "/dev/disk/by-id/scsi-0HC_Volume_";
