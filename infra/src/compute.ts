import * as hcloud from "@pulumi/hcloud";
import * as pulumi from "@pulumi/pulumi";

export function createSshKey(args: { publicKey: string }) {
  return new hcloud.SshKey("operator-key", {
    publicKey: args.publicKey,
  });
}

export function createControllerServer(args: {
  serverType: string;
  location: string;
  userData: string;
  sshKeyIds: pulumi.Output<number>[];
  firewallIds: pulumi.Output<number>[];
}) {
  return new hcloud.Server("controller", {
    serverType: args.serverType,
    location: args.location,
    image: "ubuntu-24.04",
    userData: args.userData,
    sshKeys: args.sshKeyIds.map((id) => id.apply((n) => n.toString())),
    firewallIds: args.firewallIds,
  });
}

export function createWorkerServers(args: {
  count: number;
  serverType: string;
  location: string;
  userData: pulumi.Output<string>;
  sshKeyIds: pulumi.Output<number>[];
  firewallIds: pulumi.Output<number>[];
}) {
  const servers: hcloud.Server[] = [];
  for (let i = 0; i < args.count; i++) {
    servers.push(
      new hcloud.Server(`worker-${i}`, {
        serverType: args.serverType,
        location: args.location,
        image: "ubuntu-24.04",
        userData: args.userData,
        sshKeys: args.sshKeyIds.map((id) => id.apply((n) => n.toString())),
        firewallIds: args.firewallIds,
        publicNets: [
          {
            ipv4Enabled: false,
            ipv6Enabled: true,
          },
        ],
      }),
    );
  }
  return servers;
}

export function createVolume(args: {
  sizeGb: number;
  location: string;
}) {
  return new hcloud.Volume("postgres-data", {
    size: args.sizeGb,
    location: args.location,
    format: "ext4",
  });
}

export function attachVolume(args: {
  volumeId: pulumi.Output<number>;
  serverId: pulumi.Output<number>;
}) {
  return new hcloud.VolumeAttachment("postgres-data-attachment", {
    volumeId: args.volumeId,
    serverId: args.serverId,
  });
}
