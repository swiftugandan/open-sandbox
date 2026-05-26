// Minimal proto3 encoder/decoder — only the bits the open-sandbox WS
// streaming-exec contract needs. Wire types: 0 = varint, 2 = length-
// delimited. See proto/proxy.proto for the canonical schema.

function encVarint(n: number | bigint, out: number[]) {
  let v = typeof n === "bigint" ? n : BigInt(n);
  while (v > 127n) {
    out.push(Number((v & 0x7fn) | 0x80n));
    v >>= 7n;
  }
  out.push(Number(v));
}

function decVarint(buf: Uint8Array, pos: number): [bigint, number] {
  let n = 0n;
  let shift = 0n;
  while (true) {
    const b = buf[pos++];
    n |= BigInt(b & 0x7f) << shift;
    if ((b & 0x80) === 0) break;
    shift += 7n;
  }
  return [n, pos];
}

function tag(field: number, wire: number) {
  return (field << 3) | wire;
}

function encString(field: number, s: string, out: number[]) {
  out.push(tag(field, 2));
  const bytes = new TextEncoder().encode(s);
  encVarint(bytes.length, out);
  for (const b of bytes) out.push(b);
}

function encMessage(field: number, inner: number[], out: number[]) {
  out.push(tag(field, 2));
  encVarint(inner.length, out);
  for (const b of inner) out.push(b);
}

/**
 * Encode an IoStart message carrying ExecParams.
 *
 * ```
 * IoStart { sandbox_id=1 string, oneof params { ExecParams exec=2, ... } }
 * ExecParams { repeated string command=1, string cwd=2, map<string,string> env=3 }
 * ```
 */
export function encodeIoStartExec(
  sandboxId: string,
  command: string[],
  cwd = "",
  env: Record<string, string> = {},
): Uint8Array {
  const exec: number[] = [];
  for (const arg of command) encString(1, arg, exec);
  if (cwd) encString(2, cwd, exec);
  for (const [k, v] of Object.entries(env)) {
    const entry: number[] = [];
    encString(1, k, entry);
    encString(2, v, entry);
    encMessage(3, entry, exec);
  }
  const start: number[] = [];
  encString(1, sandboxId, start);
  encMessage(2, exec, start);
  return new Uint8Array(start);
}

export function encodeIoSignal(signum: number): Uint8Array {
  const out: number[] = [];
  out.push(tag(1, 0));
  encVarint(signum, out);
  return new Uint8Array(out);
}

type WireField =
  | { wire: 0; num: number; big: bigint }
  | { wire: 2; bytes: Uint8Array };

function parseFields(buf: Uint8Array): Record<number, WireField[]> {
  const out: Record<number, WireField[]> = {};
  let pos = 0;
  while (pos < buf.length) {
    const [tagN, p1] = decVarint(buf, pos);
    pos = p1;
    const tagNum = Number(tagN);
    const field = tagNum >> 3;
    const wire = tagNum & 7;
    let val: WireField;
    if (wire === 0) {
      const [v, p2] = decVarint(buf, pos);
      pos = p2;
      val = { wire: 0, num: Number(v), big: v };
    } else if (wire === 2) {
      const [lenB, p2] = decVarint(buf, pos);
      pos = p2;
      const len = Number(lenB);
      val = { wire: 2, bytes: buf.subarray(pos, pos + len) };
      pos += len;
    } else {
      throw new Error(`unsupported wire type ${wire}`);
    }
    (out[field] ||= []).push(val);
  }
  return out;
}

const decString = (b: Uint8Array) => new TextDecoder().decode(b);

export interface IoStarted {
  execId: string;
  inContainerPid: number;
}
export function decodeIoStarted(buf: Uint8Array): IoStarted {
  const f = parseFields(buf);
  const pid = f[2]?.[0].wire === 0 ? Number(BigInt.asIntN(32, f[2][0].big)) : 0;
  return {
    execId: f[1]?.[0].wire === 2 ? decString(f[1][0].bytes) : "",
    inContainerPid: pid,
  };
}

export interface IoExited {
  exitCode: number;
  commandNotFound: boolean;
}
export function decodeIoExited(buf: Uint8Array): IoExited {
  const f = parseFields(buf);
  const exit =
    f[1]?.[0].wire === 0 ? Number(BigInt.asIntN(32, f[1][0].big)) : 0;
  const cnf = f[2]?.[0].wire === 0 ? !!f[2][0].num : false;
  return { exitCode: exit, commandNotFound: cnf };
}

export interface IoError {
  code: string;
  detail: string;
}
export function decodeIoError(buf: Uint8Array): IoError {
  const f = parseFields(buf);
  return {
    code: f[1]?.[0].wire === 2 ? decString(f[1][0].bytes) : "",
    detail: f[2]?.[0].wire === 2 ? decString(f[2][0].bytes) : "",
  };
}

// Frame envelope: [1 byte kind][protobuf payload]
export const Kind = {
  Start: 0x00,
  Stdin: 0x01,
  Signal: 0x02,
  StdinEof: 0x03,
  Stdout: 0x11,
  Stderr: 0x12,
  Exited: 0x13,
  Error: 0x14,
  Started: 0x15,
} as const;

export function frame(kind: number, payload?: Uint8Array): Uint8Array {
  const out = new Uint8Array(1 + (payload?.length ?? 0));
  out[0] = kind;
  if (payload) out.set(payload, 1);
  return out;
}
