/**
 * Type definitions matching the Arcis circuit structs
 * Field packing order MUST match exactly between TS and Rust
 */

export const MAX_INTENTS = 64;

// Field indices for Intent (36 scalars total):
// 0: user (u64)
// 1-32: hash ([u8; 32])
// 33: fee_hint (u32)
// 34: priority (u16)
// 35: reserved (u16)
export interface Intent {
  user: bigint;
  hash: Uint8Array; // 32 bytes
  feeHint: number;
  priority: number;
  reserved: number;
}

// Field indices for PlannerInput (MAX_INTENTS * 36 + 1 = 2305 scalars):
// 0-2303: intents[0..63] (each 36 scalars)
// 2304: count (u16)
export interface PlannerInput {
  intents: Intent[];
  count: number;
}

// Field indices for PlanChunk (2 scalars):
// 0: start (u16)
// 1: end (u16)
export interface PlanChunk {
  start: number;
  end: number;
}

// Field indices for PlanOutput (1 + 128 = 129 scalars):
// 0: chunk_count (u16)
// 1-128: chunks[0..63] (each 2 scalars)
export interface PlanOutput {
  chunkCount: number;
  chunks: PlanChunk[];
}

export interface CliInput {
  rpcUrl: string;
  programId: string;
  cluster: string;
  dag: PlannerInput;
  timeoutMs: number;
}

export interface CliOutput {
  ok: boolean;
  plan?: PlanOutput;
  offset?: string;
  finalizeTx?: string;
  error?: string;
  metrics?: {
    encryptMs: number;
    queueMs: number;
    awaitMs: number;
    decryptMs: number;
    totalMs: number;
  };
}

/**
 * Pack Intent into 36 field elements for Rescue cipher
 * Order: user, hash[0..31], fee_hint, priority, reserved
 * Returns number[] instead of bigint[] to avoid "Cannot mix BigInt and other types" error
 */
export function packIntent(intent: Intent): number[] {
  // Bounds checks
  const U32_MAX = 0xFFFFFFFF;
  const U16_MAX = 0xFFFF;
  const U64_MAX_NUMBER = Number.MAX_SAFE_INTEGER; // 2^53 - 1, safe for u64
  
  const userNum = typeof intent.user === 'bigint' ? Number(intent.user) : intent.user;
  
  if (userNum > U64_MAX_NUMBER) {
    throw new Error(`Intent user ${intent.user} exceeds MAX_SAFE_INTEGER`);
  }
  if (intent.feeHint > U32_MAX) {
    throw new Error(`Intent feeHint ${intent.feeHint} exceeds u32::MAX (${U32_MAX})`);
  }
  if (intent.priority > U16_MAX) {
    throw new Error(`Intent priority ${intent.priority} exceeds u16::MAX (${U16_MAX})`);
  }
  if (intent.reserved > U16_MAX) {
    throw new Error(`Intent reserved ${intent.reserved} exceeds u16::MAX (${U16_MAX})`);
  }
  if (intent.hash.length !== 32) {
    throw new Error(`Intent hash must be exactly 32 bytes, got ${intent.hash.length}`);
  }
  
  const fields: number[] = [];
  
  // Field 0: user (u64) - convert to number
  fields.push(userNum);
  
  // Fields 1-32: hash (32 bytes)
  for (let i = 0; i < 32; i++) {
    fields.push(intent.hash[i]);
  }
  
  // Field 33: fee_hint (u32)
  fields.push(intent.feeHint);
  
  // Field 34: priority (u16)
  fields.push(intent.priority);
  
  // Field 35: reserved (u16)
  fields.push(intent.reserved);
  
  return fields;
}

/**
 * Pack PlannerInput into field elements
 * Total: MAX_INTENTS * 36 + 1 = 2305 scalars
 * Returns number[] instead of bigint[] to avoid type mixing errors
 */
export function packPlannerInput(input: PlannerInput): number[] {
  // Bounds checks
  const U16_MAX = 0xFFFF;
  
  if (input.count > U16_MAX) {
    throw new Error(`PlannerInput count ${input.count} exceeds u16::MAX (${U16_MAX})`);
  }
  if (input.count > MAX_INTENTS) {
    throw new Error(`PlannerInput count ${input.count} exceeds MAX_INTENTS (${MAX_INTENTS})`);
  }
  if (input.intents.length > MAX_INTENTS) {
    throw new Error(`PlannerInput has ${input.intents.length} intents, exceeds MAX_INTENTS (${MAX_INTENTS})`);
  }
  
  const fields: number[] = [];
  
  // Pad intents to MAX_INTENTS
  const paddedIntents = [...input.intents];
  while (paddedIntents.length < MAX_INTENTS) {
    paddedIntents.push({
      user: 0n,
      hash: new Uint8Array(32),
      feeHint: 0,
      priority: 0,
      reserved: 0,
    });
  }
  
  // Pack each intent (with bounds checking)
  for (const intent of paddedIntents) {
    fields.push(...packIntent(intent));
  }
  
  // Last field: count (u16)
  fields.push(input.count);
  
  return fields;
}

/**
 * Unpack PlanOutput from field elements
 * Total: 1 + 128 = 129 scalars
 */
export function unpackPlanOutput(fields: bigint[]): PlanOutput {
  if (fields.length !== 129) {
    throw new Error(`Expected 129 fields, got ${fields.length}`);
  }
  
  let idx = 0;
  
  // Field 0: chunk_count (u16)
  const chunkCount = Number(fields[idx++]);
  
  // Fields 1-128: chunks (64 * 2)
  const chunks: PlanChunk[] = [];
  for (let i = 0; i < MAX_INTENTS; i++) {
    chunks.push({
      start: Number(fields[idx++]),
      end: Number(fields[idx++]),
    });
  }
  
  return {
    chunkCount,
    chunks: chunks.slice(0, chunkCount),
  };
}
