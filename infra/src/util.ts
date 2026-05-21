import * as pulumi from "@pulumi/pulumi";

export function idAsNumber(id: pulumi.Output<string>): pulumi.Output<number> {
  return id.apply((s) => parseInt(s, 10));
}
