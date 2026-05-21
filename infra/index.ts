import * as pulumi from "@pulumi/pulumi";
import { createSshKey, createControllerServer, createWorkerServers, createVolume, attachVolume } from "./src/compute";
import { controllerUserData, workerUserData } from "./src/cloud-init";
import { createNetwork, createFloatingIp, assignFloatingIp, attachToNetwork, createControllerFirewall, createWorkerFirewall } from "./src/networking";
import { createWildcardDns } from "./src/dns";

const config = new pulumi.Config();

const domain = config.require("domain");
const workerCount = config.getNumber("workerCount") ?? 2;
const controllerServerType = config.get("controllerServerType") ?? "cax11";
const workerServerType = config.get("workerServerType") ?? "cx22";
const volumeSizeGb = config.getNumber("volumeSizeGb") ?? 20;
const location = config.get("location") ?? "fsn1";
const networkCidr = config.get("networkCidr") ?? "10.0.0.0/16";
const subnetRange = config.get("subnetRange") ?? "10.0.0.0/24";
const operatorCidrs: string[] = JSON.parse(config.get("operatorCidrs") ?? '["0.0.0.0/0"]');
const sshPublicKey = config.require("sshPublicKey");
const cloudflareZoneId = config.get("cloudflareZoneId") ?? "";
const joinToken = config.getSecret("joinToken") ?? pulumi.output("changeme");

const sshKey = createSshKey({ publicKey: sshPublicKey });

const { network, subnet } = createNetwork({ cidr: networkCidr, subnetRange, location });

const controllerFirewall = createControllerFirewall({ operatorCidrs });
const workerFirewall = createWorkerFirewall();

const floatingIp = createFloatingIp({ location });

const databaseUrl = "postgres://postgres@127.0.0.1:5432/open_sandbox";
const controllerGrpcPort = 50051;
const proxyHttpPort = 443;
const proxyGrpcPort = 50052;

const volume = createVolume({ sizeGb: volumeSizeGb, location });

const controllerServer = createControllerServer({
  serverType: controllerServerType,
  location,
  userData: controllerUserData({
    databaseUrl,
    grpcPort: controllerGrpcPort,
    proxyHttpPort,
    proxyGrpcPort,
    volumeDevice: "/dev/disk/by-id/scsi-0HC_Volume_postgres-data",
  }),
  sshKeyIds: [sshKey.id.apply((id) => parseInt(id, 10))],
  firewallIds: [controllerFirewall.id.apply((id) => parseInt(id, 10))],
});

const volumeAttachment = attachVolume({
  volumeId: volume.id.apply((id) => parseInt(id, 10)),
  serverId: controllerServer.id.apply((id) => parseInt(id, 10)),
});

const floatingIpAssignment = assignFloatingIp({
  floatingIpId: floatingIp.id.apply((id) => parseInt(id, 10)),
  serverId: controllerServer.id.apply((id) => parseInt(id, 10)),
});

const controllerPrivateIp = "10.0.0.2";
const controllerNetAttachment = attachToNetwork("controller-net", {
  serverId: controllerServer.id.apply((id) => parseInt(id, 10)),
  networkId: network.id.apply((id) => parseInt(id, 10)),
  ip: controllerPrivateIp,
});

const workerServers = createWorkerServers({
  count: workerCount,
  serverType: workerServerType,
  location,
  userData: workerUserData({
    controllerUrl: `http://${controllerPrivateIp}:${controllerGrpcPort}`,
    proxyUrl: `http://${controllerPrivateIp}:${proxyGrpcPort}`,
    joinToken,
  }),
  sshKeyIds: [sshKey.id.apply((id) => parseInt(id, 10))],
  firewallIds: [workerFirewall.id.apply((id) => parseInt(id, 10))],
});

const workerNetAttachments = workerServers.map((server, i) =>
  attachToNetwork(`worker-${i}-net`, {
    serverId: server.id.apply((id) => parseInt(id, 10)),
    networkId: network.id.apply((id) => parseInt(id, 10)),
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
