import { describe, it, expect, beforeAll } from "vitest";
import * as pulumi from "@pulumi/pulumi";

interface MockResource {
  name: string;
  type: string;
  inputs: Record<string, any>;
}

const resources: MockResource[] = [];

pulumi.runtime.setAllConfig({
  "open-sandbox:domain": "example.com",
  "open-sandbox:workerCount": "2",
  "open-sandbox:controllerServerType": "cax11",
  "open-sandbox:workerServerType": "cx22",
  "open-sandbox:volumeSizeGb": "20",
  "open-sandbox:location": "fsn1",
  "open-sandbox:networkCidr": "10.0.0.0/16",
  "open-sandbox:subnetRange": "10.0.0.0/24",
  "open-sandbox:operatorCidrs": '["0.0.0.0/0"]',
  "open-sandbox:sshPublicKey": "ssh-ed25519 AAAA test-key",
});

pulumi.runtime.setMocks(
  {
    newResource: (args: pulumi.runtime.MockResourceArgs) => {
      resources.push({
        name: args.name,
        type: args.type,
        inputs: args.inputs,
      });
      return {
        id: `${args.name}-id`,
        state: { ...args.inputs, id: `${args.name}-id` },
      };
    },
    call: (args: pulumi.runtime.MockCallArgs) => {
      return args.inputs;
    },
  },
  "open-sandbox",
  "dev",
  false,
);

const byType = (type: string) => resources.filter((r) => r.type === type);
const byNameAndType = (nameSubstr: string, type: string) =>
  resources.filter((r) => r.type === type && r.name.includes(nameSubstr));

describe("open-sandbox infrastructure", () => {
  beforeAll(async () => {
    await import("../index");
  });

  // ── Controller VM ──────────────────────────────────────────────

  describe("controller VM", () => {
    it("creates exactly one controller server", () => {
      const controllers = byNameAndType(
        "controller",
        "hcloud:index/server:Server",
      );
      expect(controllers).toHaveLength(1);
    });

    it("uses a 2-vCPU / 4-GB server type", () => {
      const [ctrl] = byNameAndType(
        "controller",
        "hcloud:index/server:Server",
      );
      expect(ctrl).toBeDefined();
      expect(["cax11", "cx22"]).toContain(ctrl.inputs.serverType);
    });

    it("is provisioned in the configured location", () => {
      const [ctrl] = byNameAndType(
        "controller",
        "hcloud:index/server:Server",
      );
      expect(ctrl.inputs.location).toBe("fsn1");
    });

    it("has cloud-init user data that starts controller and proxy", () => {
      const [ctrl] = byNameAndType(
        "controller",
        "hcloud:index/server:Server",
      );
      expect(ctrl.inputs.userData).toBeDefined();
      expect(ctrl.inputs.userData).toContain("open-sandbox controller");
      expect(ctrl.inputs.userData).toContain("open-sandbox proxy");
    });

    it("cloud-init installs and configures Postgres", () => {
      const [ctrl] = byNameAndType(
        "controller",
        "hcloud:index/server:Server",
      );
      expect(ctrl.inputs.userData).toContain("postgresql");
    });

    it("cloud-init sets up pg_dump backup cron", () => {
      const [ctrl] = byNameAndType(
        "controller",
        "hcloud:index/server:Server",
      );
      expect(ctrl.inputs.userData).toContain("pg_dump");
    });
  });

  // ── Worker VMs ─────────────────────────────────────────────────

  describe("worker VMs", () => {
    it("creates the configured number of workers (default 2)", () => {
      const workers = byNameAndType("worker", "hcloud:index/server:Server");
      expect(workers).toHaveLength(2);
    });

    it("workers have no public IPv4", () => {
      const workers = byNameAndType("worker", "hcloud:index/server:Server");
      expect(workers.length).toBeGreaterThan(0);
      for (const w of workers) {
        const pubNet = w.inputs.publicNets;
        expect(pubNet).toBeDefined();
        const ipv4Setting = pubNet.find(
          (p: any) => p.ipv4Enabled !== undefined,
        );
        expect(ipv4Setting?.ipv4Enabled).toBe(false);
      }
    });

    it("worker cloud-init starts agent with join token", () => {
      const workers = byNameAndType("worker", "hcloud:index/server:Server");
      expect(workers.length).toBeGreaterThan(0);
      for (const w of workers) {
        expect(w.inputs.userData).toContain("open-sandbox agent");
        expect(w.inputs.userData).toContain("OPEN_SANDBOX_JOIN_TOKEN");
      }
    });
  });

  // ── Private Network ────────────────────────────────────────────

  describe("private network", () => {
    it("creates a Hetzner Cloud Network", () => {
      const nets = byType("hcloud:index/network:Network");
      expect(nets).toHaveLength(1);
    });

    it("creates a subnet", () => {
      const subnets = byType("hcloud:index/networkSubnet:NetworkSubnet");
      expect(subnets).toHaveLength(1);
    });

    it("attaches controller and all workers to the network", () => {
      const attachments = byType("hcloud:index/serverNetwork:ServerNetwork");
      // 1 controller + 2 workers = 3
      expect(attachments).toHaveLength(3);
    });
  });

  // ── Floating IP ────────────────────────────────────────────────

  describe("floating IP", () => {
    it("creates one IPv4 floating IP", () => {
      const fips = byType("hcloud:index/floatingIp:FloatingIp");
      expect(fips).toHaveLength(1);
      expect(fips[0].inputs.type).toBe("ipv4");
    });

    it("assigns floating IP to the controller", () => {
      const assignments = byType(
        "hcloud:index/floatingIpAssignment:FloatingIpAssignment",
      );
      expect(assignments).toHaveLength(1);
    });
  });

  // ── Firewalls ──────────────────────────────────────────────────

  describe("firewalls", () => {
    it("creates a controller firewall allowing only TCP 443 and 22 inbound", () => {
      const fws = byNameAndType("controller", "hcloud:index/firewall:Firewall");
      expect(fws).toHaveLength(1);

      const rules: any[] = fws[0].inputs.rules;
      const inbound = rules.filter((r) => r.direction === "in");
      const ports = inbound.map((r) => r.port).sort();
      expect(ports).toEqual(["22", "443"]);
      for (const r of inbound) {
        expect(r.protocol).toBe("tcp");
      }
    });

    it("creates a worker firewall with no inbound rules", () => {
      const fws = byNameAndType("worker", "hcloud:index/firewall:Firewall");
      expect(fws).toHaveLength(1);

      const rules: any[] = fws[0].inputs.rules;
      const inbound = rules.filter((r) => r.direction === "in");
      expect(inbound).toHaveLength(0);
    });
  });

  // ── Block Volume ───────────────────────────────────────────────

  describe("block volume", () => {
    it("creates a 20 GB volume for Postgres data", () => {
      const vols = byType("hcloud:index/volume:Volume");
      expect(vols).toHaveLength(1);
      expect(vols[0].inputs.size).toBe(20);
    });

    it("attaches volume to controller", () => {
      const attachments = byType(
        "hcloud:index/volumeAttachment:VolumeAttachment",
      );
      expect(attachments).toHaveLength(1);
    });
  });

  // ── DNS ────────────────────────────────────────────────────────

  describe("DNS", () => {
    it("creates a Cloudflare wildcard A record for sandbox subdomains", () => {
      const records = byType("cloudflare:index/record:Record");
      const wildcard = records.find((r) =>
        r.inputs.name?.includes("*"),
      );
      expect(wildcard).toBeDefined();
      expect(wildcard!.inputs.type).toBe("A");
    });
  });

  // ── SSH Key ────────────────────────────────────────────────────

  describe("SSH key", () => {
    it("registers an SSH public key", () => {
      const keys = byType("hcloud:index/sshKey:SshKey");
      expect(keys).toHaveLength(1);
    });
  });
});
