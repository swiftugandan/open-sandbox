import * as cloudflare from "@pulumi/cloudflare";
import * as pulumi from "@pulumi/pulumi";

export function createWildcardDns(args: {
  zoneId: string;
  domain: string;
  floatingIpAddress: pulumi.Output<string>;
}) {
  return new cloudflare.Record("sandbox-wildcard", {
    zoneId: args.zoneId,
    name: `*.sandbox.${args.domain}`,
    type: "A",
    content: args.floatingIpAddress,
    proxied: false,
    ttl: 300,
  });
}
