import * as hcloud from "@pulumi/hcloud";
import * as pulumi from "@pulumi/pulumi";

export function createNetwork(args: {
  cidr: string;
  subnetRange: string;
  location: string;
}) {
  const network = new hcloud.Network("platform-network", {
    ipRange: args.cidr,
  });

  const subnet = new hcloud.NetworkSubnet("platform-subnet", {
    networkId: network.id.apply((id) => parseInt(id, 10)),
    type: "cloud",
    networkZone: locationToZone(args.location),
    ipRange: args.subnetRange,
  });

  return { network, subnet };
}

export function createFloatingIp(args: { location: string }) {
  const floatingIp = new hcloud.FloatingIp("controller-ip", {
    type: "ipv4",
    homeLocation: args.location,
  });

  return floatingIp;
}

export function assignFloatingIp(args: {
  floatingIpId: pulumi.Output<number>;
  serverId: pulumi.Output<number>;
}) {
  return new hcloud.FloatingIpAssignment("controller-ip-assignment", {
    floatingIpId: args.floatingIpId,
    serverId: args.serverId,
  });
}

export function attachToNetwork(
  name: string,
  args: {
    serverId: pulumi.Output<number>;
    networkId: pulumi.Output<number>;
    ip?: string;
  },
) {
  return new hcloud.ServerNetwork(name, {
    serverId: args.serverId,
    networkId: args.networkId,
    ip: args.ip,
  });
}

export function createControllerFirewall(args: {
  operatorCidrs: string[];
}) {
  return new hcloud.Firewall("controller-firewall", {
    rules: [
      {
        direction: "in",
        protocol: "tcp",
        port: "443",
        sourceIps: ["0.0.0.0/0", "::/0"],
      },
      {
        direction: "in",
        protocol: "tcp",
        port: "22",
        sourceIps: args.operatorCidrs,
      },
    ],
  });
}

export function createWorkerFirewall() {
  return new hcloud.Firewall("worker-firewall", {
    rules: [],
  });
}

function locationToZone(location: string): string {
  if (location.startsWith("fsn") || location.startsWith("nbg")) {
    return "eu-central";
  }
  if (location.startsWith("hel")) {
    return "eu-central";
  }
  if (location.startsWith("ash") || location.startsWith("hil")) {
    return "us-east";
  }
  return "eu-central";
}
