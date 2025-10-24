/**
 * @internal
 * Internal parity builder - mirrors bundler/src/serializer.rs algorithm
 * for deterministic byte-for-byte equality with Rust golden outputs.
 *
 * DO NOT EXPORT from public API - this is only for golden tests.
 */

import {
  PublicKey,
  TransactionInstruction,
  MessageV0,
  MessageHeader,
  MessageCompiledInstruction,
  MessageAddressTableLookup,
} from '@solana/web3.js';
import crypto from 'node:crypto';

interface ParityMessageOpts {
  payer: PublicKey;
  recentBlockhash: string;
  cuLimit: number;
  cuPriceMicrolamports: number;
  userIxs: TransactionInstruction[]; // transfer + optional memo
  lookups?: {
    tablePubkey: PublicKey;
    writableIndexes: number[];
    readonlyIndexes: number[];
  }[];
}

/** Account classification for v0 message construction. */
interface AccountKey {
  pubkey: PublicKey;
  isSigner: boolean;
  isWritable: boolean;
}

/** Deterministically derive ALT address: sha256(tablePubkey || index_byte). */
function deriveAltAddress(table: PublicKey, index: number): PublicKey {
  const hasher = crypto.createHash('sha256');
  hasher.update(table.toBytes());
  hasher.update(Buffer.from([index & 0xff]));
  const out = hasher.digest();
  return new PublicKey(out.subarray(0, 32));
}

/** Build an ALT-anchor instruction that references all declared LUT indices. */
function buildAltAnchorInstruction(lookups: Required<ParityMessageOpts>['lookups']): TransactionInstruction | null {
  if (!lookups || lookups.length === 0) return null;

  const COMPUTE_BUDGET_PROGRAM_ID = new PublicKey('ComputeBudget111111111111111111111111111111');

  const metas: { pubkey: PublicKey; isSigner: boolean; isWritable: boolean }[] = [];
  for (const l of lookups) {
    const maxIdx = Math.max(-1, ...l.writableIndexes, ...l.readonlyIndexes);
    if (maxIdx < 0) continue;
    for (const i of l.writableIndexes) {
      metas.push({ pubkey: deriveAltAddress(l.tablePubkey, i), isSigner: false, isWritable: true });
    }
    for (const i of l.readonlyIndexes) {
      metas.push({ pubkey: deriveAltAddress(l.tablePubkey, i), isSigner: false, isWritable: false });
    }
  }

  if (metas.length === 0) return null;

  return new TransactionInstruction({
    keys: metas.map(m => ({ pubkey: m.pubkey, isSigner: m.isSigner, isWritable: m.isWritable })),
    programId: COMPUTE_BUDGET_PROGRAM_ID,
    data: Buffer.alloc(0), // no-op
  });
}

/** Compare two PublicKeys by byte value. */
function comparePubkeys(a: PublicKey, b: PublicKey): number {
  const ab = a.toBytes();
  const bb = b.toBytes();
  for (let i = 0; i < 32; i++) {
    if (ab[i] !== bb[i]) return ab[i] - bb[i];
  }
  return 0;
}

/**
 * Build a v0 message with deterministic account ordering that matches
 * Rust's MessageV0::try_compile() behavior.
 *
 * Account ordering (static portion):
 * 1. Fee payer (signer + writable, first)
 * 2. Other signer writables
 * 3. Other signer readonlys
 * 4. Non-signer writables
 * 5. Non-signer readonlys (programs), all sorted bytewise within categories.
 *
 * Addresses that are referenced via LUT are EXCLUDED from staticAccountKeys
 * and appear in the combined key list after the static keys. Within each LUT,
 * the order is writableIndexes first, then readonlyIndexes (each preserving index order).
 */
export function buildMessageV0Parity(opts: ParityMessageOpts): MessageV0 {
  const { payer, recentBlockhash, cuLimit, cuPriceMicrolamports } = opts;
  const lookups = opts.lookups ?? [];

  // 1. Compute budget instructions (limit -> price)
  const cuLimitIx = createComputeBudgetLimitInstruction(cuLimit);
  const cuPriceIx = createComputeBudgetPriceInstruction(cuPriceMicrolamports);

  // 1a. Optional ALT anchor ix so lookups are actually used
  const altAnchorIx = buildAltAnchorInstruction(lookups);

  const allIxs: TransactionInstruction[] = [cuLimitIx, cuPriceIx, ...opts.userIxs];
  if (altAnchorIx) allIxs.push(altAnchorIx);

  // 2. Pre-compute the set of looked-up addresses for exclusion from static keys
  const lookedUpWritable = new Set<string>();
  const lookedUpReadonly = new Set<string>();
  for (const l of lookups) {
    for (const i of l.writableIndexes) {
      lookedUpWritable.add(deriveAltAddress(l.tablePubkey, i).toBase58());
    }
    for (const i of l.readonlyIndexes) {
      lookedUpReadonly.add(deriveAltAddress(l.tablePubkey, i).toBase58());
    }
  }
  const lookedUpAll = new Set<string>([...lookedUpWritable, ...lookedUpReadonly]);

  // 3. Collect static account keys from instructions, excluding looked-up ones
  const accountKeysMap = new Map<string, AccountKey>();

  for (const ix of allIxs) {
    // Program id is readonly non-signer; always static
    const pid = ix.programId.toBase58();
    if (!accountKeysMap.has(pid)) {
      accountKeysMap.set(pid, { pubkey: ix.programId, isSigner: false, isWritable: false });
    }

    for (const meta of ix.keys) {
      const k = meta.pubkey.toBase58();
      // If this key is looked up, exclude from static set
      if (lookedUpAll.has(k)) continue;

      const existing = accountKeysMap.get(k);
      if (existing) {
        existing.isSigner = existing.isSigner || meta.isSigner;
        existing.isWritable = existing.isWritable || meta.isWritable;
      } else {
        accountKeysMap.set(k, { pubkey: meta.pubkey, isSigner: meta.isSigner, isWritable: meta.isWritable });
      }
    }
  }

  // Ensure payer is signer+writable static and first
  const payerKey = payer.toBase58();
  const payerAcc = accountKeysMap.get(payerKey);
  if (payerAcc) {
    payerAcc.isSigner = true;
    payerAcc.isWritable = true;
  } else {
    accountKeysMap.set(payerKey, { pubkey: payer, isSigner: true, isWritable: true });
  }

  // 4. Partition and sort static accounts
  const accounts = Array.from(accountKeysMap.values());

  const signerWritables: AccountKey[] = [];
  const signerReadonlys: AccountKey[] = [];
  const nonSignerWritables: AccountKey[] = [];
  const nonSignerReadonlys: AccountKey[] = [];

  for (const acc of accounts) {
    if (acc.isSigner && acc.isWritable) signerWritables.push(acc);
    else if (acc.isSigner) signerReadonlys.push(acc);
    else if (acc.isWritable) nonSignerWritables.push(acc);
    else nonSignerReadonlys.push(acc);
  }

  signerWritables.sort((a, b) => {
    if (a.pubkey.equals(payer)) return -1;
    if (b.pubkey.equals(payer)) return 1;
    return comparePubkeys(a.pubkey, b.pubkey);
  });
  signerReadonlys.sort((a, b) => comparePubkeys(a.pubkey, b.pubkey));
  nonSignerWritables.sort((a, b) => comparePubkeys(a.pubkey, b.pubkey));
  nonSignerReadonlys.sort((a, b) => comparePubkeys(a.pubkey, b.pubkey));

  const staticAccountKeys = [
    ...signerWritables.map(a => a.pubkey),
    ...signerReadonlys.map(a => a.pubkey),
    ...nonSignerWritables.map(a => a.pubkey),
    ...nonSignerReadonlys.map(a => a.pubkey),
  ];

  // 5. Construct looked-up keys in the same order as Rust serialization:
  // for each lookup table (in given order): all writableIndexes, then readonlyIndexes
  const lookedUpOrder: PublicKey[] = [];
  for (const l of lookups) {
    for (const i of l.writableIndexes) lookedUpOrder.push(deriveAltAddress(l.tablePubkey, i));
    for (const i of l.readonlyIndexes) lookedUpOrder.push(deriveAltAddress(l.tablePubkey, i));
  }

  // 6. Build global index map: static first, then all looked-up keys
  const indexMap = new Map<string, number>();
  staticAccountKeys.forEach((pk, i) => indexMap.set(pk.toBase58(), i));
  const staticLen = staticAccountKeys.length;
  lookedUpOrder.forEach((pk, i) => indexMap.set(pk.toBase58(), staticLen + i));

  // 7. Compile instructions to refer into the combined key set
  const compiledInstructions: MessageCompiledInstruction[] = allIxs.map(ix => {
    const programIdIndex = indexMap.get(ix.programId.toBase58());
    if (programIdIndex === undefined) {
      throw new Error(`Program ID ${ix.programId.toBase58()} not in account keys`);
    }

    const accountKeyIndexes = ix.keys.map(meta => {
      const idx = indexMap.get(meta.pubkey.toBase58());
      if (idx === undefined) {
        throw new Error(`Account ${meta.pubkey.toBase58()} not in account keys`);
      }
      return idx;
    });

    return { programIdIndex, accountKeyIndexes, data: ix.data };
  });

  // 8. Header counts reflect ONLY static accounts (as in v0)
  const header: MessageHeader = {
    numRequiredSignatures: signerWritables.length + signerReadonlys.length,
    numReadonlySignedAccounts: signerReadonlys.length,
    numReadonlyUnsignedAccounts: nonSignerReadonlys.length,
  };

  // 9. LUT metadata must be number[] (not Uint8Array)
  const addressTableLookups: MessageAddressTableLookup[] = lookups.map(l => ({
    accountKey: l.tablePubkey,
    writableIndexes: l.writableIndexes,
    readonlyIndexes: l.readonlyIndexes,
  }));

  // 10. Construct MessageV0
  return new MessageV0({
    header,
    staticAccountKeys,
    recentBlockhash,
    compiledInstructions,
    addressTableLookups,
  });
}

/** ComputeBudget helpers (stable program ID on chain). */
function createComputeBudgetLimitInstruction(units: number): TransactionInstruction {
  const COMPUTE_BUDGET_PROGRAM_ID = new PublicKey('ComputeBudget111111111111111111111111111111');
  const data = Buffer.alloc(5);
  data.writeUint8(2, 0);          // SetComputeUnitLimit opcode
  data.writeUInt32LE(units >>> 0, 1);
  return new TransactionInstruction({ keys: [], programId: COMPUTE_BUDGET_PROGRAM_ID, data });
}

function createComputeBudgetPriceInstruction(microLamports: number): TransactionInstruction {
  const COMPUTE_BUDGET_PROGRAM_ID = new PublicKey('ComputeBudget111111111111111111111111111111');
  const data = Buffer.alloc(9);
  data.writeUint8(3, 0);          // SetComputeUnitPrice opcode
  data.writeBigUInt64LE(BigInt(microLamports), 1);
  return new TransactionInstruction({ keys: [], programId: COMPUTE_BUDGET_PROGRAM_ID, data });
}
