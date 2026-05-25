import * as cloudflare from "@pulumi/cloudflare";
import * as pulumi from "@pulumi/pulumi";

export function createWildcardDns(args: {
  zoneId: string;
  domain: string;
  floatingIpAddress: pulumi.Output<string>;
}) {
  // Comp-9 #8: proxied = true so Cloudflare's edge handles wildcard
  // HTTPS (free Universal SSL covers *.sandbox.<domain> automatically)
  // and the Hetzner origin IP hides behind Cloudflare's DDoS scrubbing.
  // Cloudflare → origin can stay plaintext since the hop runs over
  // Cloudflare's network; for end-to-end TLS, configure Cloudflare's
  // "Full (strict)" SSL mode + a Cloudflare Origin certificate.
  //
  // TTL is set to 1 (Cloudflare auto-manages) when proxied — passing
  // any other TTL errors out at apply time.
  return new cloudflare.Record("sandbox-wildcard", {
    zoneId: args.zoneId,
    name: `*.sandbox.${args.domain}`,
    type: "A",
    content: args.floatingIpAddress,
    proxied: true,
    ttl: 1,
  });
}
