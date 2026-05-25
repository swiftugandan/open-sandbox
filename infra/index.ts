import * as pulumi from "@pulumi/pulumi";
import { createSshKey, createControllerServer, createWorkerServers, createVolume, attachVolume } from "./src/compute";
import { controllerUserData, workerUserData } from "./src/cloud-init";
import { createNetwork, createFloatingIp, assignFloatingIp, attachToNetwork, createControllerFirewall, createWorkerFirewall } from "./src/networking";
import { createWildcardDns } from "./src/dns";
import { CONTROLLER_GRPC_PORT, PROXY_HTTP_PORT, PROXY_GRPC_PORT, PROXY_INTERNAL_GRPC_PORT, DATABASE_URL, CONTROLLER_PRIVATE_IP, VOLUME_DEVICE_PREFIX } from "./src/constants";
import { idAsNumber } from "./src/util";

const config = new pulumi.Config();

const domain = config.require("domain");
const workerCount = config.getNumber("workerCount") ?? 2;
const controllerServerType = config.get("controllerServerType") ?? "cax11";
const workerServerType = config.get("workerServerType") ?? "cx22";
const volumeSizeGb = config.getNumber("volumeSizeGb") ?? 20;
const location = config.get("location") ?? "fsn1";
const networkCidr = config.get("networkCidr") ?? "10.0.0.0/16";
const subnetRange = config.get("subnetRange") ?? "10.0.0.0/24";
// Comp-9: operatorCidrs MUST be set explicitly. Previously defaulted to
// "0.0.0.0/0", exposing SSH on the controller to the entire internet on
// any stack that forgot to override.
const operatorCidrsRaw = config.require("operatorCidrs");
const operatorCidrs: string[] = JSON.parse(operatorCidrsRaw);
if (operatorCidrs.includes("0.0.0.0/0")) {
    pulumi.log.warn(
        "operatorCidrs contains 0.0.0.0/0 — controller SSH is open to the entire internet. Set a real CIDR or accept the risk explicitly.",
    );
}
const sshPublicKey = config.require("sshPublicKey");
const cloudflareZoneId = config.get("cloudflareZoneId") ?? "";
// Comp-9: joinToken MUST be set via `pulumi config set --secret joinToken ...`.
// Previously fell back to the literal string "changeme", which workers
// were provisioned with whenever the operator forgot to set the secret.
const joinToken = config.requireSecret("joinToken");

// Comp-9: the auth tokens the comp-1/2 review round added. The binaries
// REFUSE TO START without these, so cloud-init must supply them.
// Required secrets:
//   pulumi config set --secret controllerAdminToken <random>
//   pulumi config set --secret internalToken <random>
//   pulumi config set --secret tunnelJoinToken <random>
//   pulumi config set --secret apiKey <random>
const controllerAdminToken = config.requireSecret("controllerAdminToken");
const internalToken = config.requireSecret("internalToken");
const tunnelJoinToken = config.requireSecret("tunnelJoinToken");
const apiKey = config.requireSecret("apiKey");
// Comp-9: postgres password baked into the DATABASE_URL. Operator
// generates with e.g. `openssl rand -hex 32` and sets via
//   pulumi config set --secret dbPassword <value>
// cloud-init runs `ALTER USER postgres PASSWORD '...'` before any
// binary starts. Without this, postgres is trust-on-127.0.0.1 and
// any process on the host is superuser.
const dbPassword = config.requireSecret("dbPassword");

// Comp-2 C5 / comp-9 #1: optional ACME settings for the proxy's public
// listener. When set, the proxy issues a Let's Encrypt cert via
// TLS-ALPN-01. Set both or neither.
const tunnelAcmeDomain = config.get("tunnelAcmeDomain"); // e.g. tunnel.<domain>
const acmeEmail = config.get("acmeEmail");

const sshKey = createSshKey({ publicKey: sshPublicKey });

const { network, subnet } = createNetwork({ cidr: networkCidr, subnetRange, location });

const controllerFirewall = createControllerFirewall({ operatorCidrs });
const workerFirewall = createWorkerFirewall();

const floatingIp = createFloatingIp({ location });

const volumeName = "postgres-data";
const volume = createVolume({ sizeGb: volumeSizeGb, location });

const controllerServer = createControllerServer({
  serverType: controllerServerType,
  location,
  userData: controllerUserData({
    // Comp-9: dbPassword baked into DATABASE_URL at apply time. Pulumi
    // pulumi.interpolate keeps the secret marked in state.
    databaseUrl: pulumi.interpolate`postgres://postgres:${dbPassword}@127.0.0.1:5432/open_sandbox`,
    dbPassword,
    grpcPort: CONTROLLER_GRPC_PORT,
    proxyHttpPort: PROXY_HTTP_PORT,
    proxyGrpcPort: PROXY_GRPC_PORT,
    proxyInternalGrpcPort: PROXY_INTERNAL_GRPC_PORT,
    volumeDevice: `${VOLUME_DEVICE_PREFIX}${volumeName}`,
    controllerAdminToken,
    internalToken,
    tunnelJoinToken,
    apiKey,
    tunnelAcmeDomain,
    acmeEmail,
  }),
  sshKeyIds: [idAsNumber(sshKey.id)],
  firewallIds: [idAsNumber(controllerFirewall.id)],
});

const volumeAttachment = attachVolume({
  volumeId: idAsNumber(volume.id),
  serverId: idAsNumber(controllerServer.id),
});

const floatingIpAssignment = assignFloatingIp({
  floatingIpId: idAsNumber(floatingIp.id),
  serverId: idAsNumber(controllerServer.id),
});

const controllerNetAttachment = attachToNetwork("controller-net", {
  serverId: idAsNumber(controllerServer.id),
  networkId: idAsNumber(network.id),
  ip: CONTROLLER_PRIVATE_IP,
});

const workerServers = createWorkerServers({
  count: workerCount,
  serverType: workerServerType,
  location,
  userData: workerUserData({
    controllerUrl: `http://${CONTROLLER_PRIVATE_IP}:${CONTROLLER_GRPC_PORT}`,
    proxyUrl: `http://${CONTROLLER_PRIVATE_IP}:${PROXY_GRPC_PORT}`,
    joinToken,
    tunnelJoinToken,
  }),
  sshKeyIds: [idAsNumber(sshKey.id)],
  firewallIds: [idAsNumber(workerFirewall.id)],
});

const workerNetAttachments = workerServers.map((server, i) =>
  attachToNetwork(`worker-${i}-net`, {
    serverId: idAsNumber(server.id),
    networkId: idAsNumber(network.id),
  }),
);

const wildcardDns = createWildcardDns({
  zoneId: cloudflareZoneId,
  domain,
  floatingIpAddress: floatingIp.ipAddress,
});

export {
  controllerServer,
  workerServers,
  floatingIp,
  floatingIpAssignment,
  volume,
  volumeAttachment,
  network,
  subnet,
  controllerFirewall,
  workerFirewall,
  sshKey,
  wildcardDns,
};
