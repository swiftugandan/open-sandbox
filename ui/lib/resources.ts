// Resource tier presets surfaced in the create form. The platform
// default (controller's DEFAULT_SANDBOX_CPU_MILLICORES = 1000, and
// DEFAULT_SANDBOX_MEMORY_BYTES = 512 MiB) is "medium" — picking that
// tier sends no override on the wire so the controller stays in
// charge of the default. The other tiers send explicit values.

export interface ResourceTier {
  id: "small" | "medium" | "large";
  label: string;
  cpuMillicores: number;
  memoryBytes: number;
  description: string;
  /** True iff this tier's values match the controller defaults — when
   *  selected, we omit cpu_millicores / memory_bytes from the wire. */
  isPlatformDefault: boolean;
}

const MIB = 1024 * 1024;

// The Small / Large rows are sized relative to the platform default
// (whatever it happens to be today) and we send those values
// explicitly on the wire. Their descriptions name the values because
// the UI *is* the source of truth for those numbers — picking Small
// always means 0.5 vCPU / 256 MiB regardless of contract changes.
//
// Medium is different: it's "whatever the platform default is" and
// gets sent with NO override on the wire. The description must NOT
// mention specific numbers — if `DEFAULT_SANDBOX_CPU_MILLICORES` /
// `DEFAULT_SANDBOX_MEMORY_BYTES` change in `crates/contracts/`, a
// hard-coded "1 vCPU · 512 MiB" here would silently lie. So Medium
// reads as "platform default" only.
export const RESOURCE_TIERS: readonly ResourceTier[] = [
  {
    id: "small",
    label: "Small",
    cpuMillicores: 500,
    memoryBytes: 256 * MIB,
    description: "0.5 vCPU · 256 MiB. Light tasks, simple servers.",
    isPlatformDefault: false,
  },
  {
    id: "medium",
    label: "Medium",
    cpuMillicores: 1000,
    memoryBytes: 512 * MIB,
    description: "Platform default. Tuned by the platform.",
    isPlatformDefault: true,
  },
  {
    id: "large",
    label: "Large",
    cpuMillicores: 2000,
    memoryBytes: 1024 * MIB,
    description: "2 vCPU · 1 GiB. Heavier builds, multiple services.",
    isPlatformDefault: false,
  },
] as const;

export const DEFAULT_RESOURCE_TIER_ID: ResourceTier["id"] = "medium";

export function findTier(id: string): ResourceTier | undefined {
  return RESOURCE_TIERS.find((t) => t.id === id);
}
