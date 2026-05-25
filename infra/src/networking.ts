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
      // Public HTTPS for sandbox traffic (`*.sandbox.<domain>`).
      // Cloudflare proxied → origin: connections come from Cloudflare
      // edges, but Hetzner firewalls work at L4 so we still open :443
      // wide. Tighten with Cloudflare's IP ranges if you want belt-
      // and-braces.
      {
        direction: "in",
        protocol: "tcp",
        port: "443",
        sourceIps: ["0.0.0.0/0", "::/0"],
      },
      // OpenTunnel public listener for BYO agents dialing in. Public
      // by design (BYO-from-anywhere); auth gated by TUNNEL_JOIN_TOKEN
      // (comp-2 A1) and ACME-managed TLS when enabled (comp-2 C5).
      {
        direction: "in",
        protocol: "tcp",
        port: "50052",
        sourceIps: ["0.0.0.0/0", "::/0"],
      },
      // SSH — restricted to operator CIDRs (comp-9 fail-closed).
      {
        direction: "in",
        protocol: "tcp",
        port: "22",
        sourceIps: args.operatorCidrs,
      },
      // 50051 (controller gRPC) + 50053 (proxy internal gRPC) stay on
      // the private Hetzner network — not exposed publicly.
    ],
  });
}

export function createWorkerFirewall() {
  return new hcloud.Firewall("worker-firewall", {
    rules: [],
  });
}

function locationToZone(location: string): string {
  if (location.startsWith("ash") || location.startsWith("hil")) {
    return "us-east";
  }
  return "eu-central";
}
