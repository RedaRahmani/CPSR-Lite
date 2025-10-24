/**
 * Golden Parity Tests: SDK vs Rust Serializer
 */

import {
  PublicKey,
  SystemProgram,
  TransactionInstruction,
} from '@solana/web3.js';
import { createMemoInstruction } from '@solana/spl-memo';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildMessageV0Parity } from '../../src/tx/serializer_parity.js';
import { runRustGolden, readGolden, type GoldenTestCase } from '../../src/test-utils/run-rust-golden.js';

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

/** Build user instructions (transfer + optional memo). */
function buildUserInstructions(golden: GoldenTestCase): TransactionInstruction[] {
  const userIxs: TransactionInstruction[] = [];

  for (const transfer of golden.transfers) {
    const from = new PublicKey(transfer.from);
    const to = new PublicKey(transfer.to);
    userIxs.push(
      SystemProgram.transfer({
        fromPubkey: from,
        toPubkey: to,
        lamports: transfer.lamports,
      })
    );
  }

  if (golden.memo) {
    userIxs.push(createMemoInstruction(golden.memo, []));
  }

  return userIxs;
}

function findFirstDiff(a: Uint8Array, b: Uint8Array): number | null {
  const len = Math.min(a.length, b.length);
  for (let i = 0; i < len; i++) {
    if (a[i] !== b[i]) return i;
  }
  return a.length !== b.length ? len : null;
}

function formatBytesAround(bytes: Uint8Array, index: number, context = 8): string {
  const start = Math.max(0, index - context);
  const end = Math.min(bytes.length, index + context + 1);
  const slice = bytes.slice(start, end);
  const hex = Array.from(slice).map((b, i) => {
    const pos = start + i;
    const formatted = b.toString(16).padStart(2, '0');
    return pos === index ? `[${formatted}]` : formatted;
  }).join(' ');
  return `offset ${start}: ${hex}`;
}

describe('Golden Parity: SDK vs Rust Serializer', () => {
  const goldensDir = join(__dirname, '../../__goldens__');

  it('g1_transfer: byte-for-byte equality on simple SOL transfer', async () => {
    const goldenPath = join(goldensDir, 'g1_transfer.json');
    const golden = await readGolden(goldenPath);

    const userIxs = buildUserInstructions(golden);

    const messageV0 = buildMessageV0Parity({
      payer: new PublicKey(golden.payer),
      recentBlockhash: golden.recentBlockhash,
      cuLimit: golden.cuLimit,
      cuPriceMicrolamports: golden.cuPriceMicrolamports,
      userIxs,
      lookups: golden.alts.lookups.map(l => ({
        tablePubkey: new PublicKey(l.tablePubkey),
        writableIndexes: l.writableIndexes,
        readonlyIndexes: l.readonlyIndexes,
      })),
    });

    const sdkBytes = messageV0.serialize();
    const sdkBase64 = Buffer.from(sdkBytes).toString('base64');

    const rustOutput = await runRustGolden(goldenPath);
    const rustBytes = Buffer.from(rustOutput.message_base64, 'base64');

    if (sdkBase64 !== rustOutput.message_base64) {
      const diffIndex = findFirstDiff(sdkBytes, rustBytes) ?? 0;
      const sdkContext = formatBytesAround(sdkBytes, diffIndex);
      const rustContext = formatBytesAround(rustBytes, diffIndex);
      throw new Error(
        `Byte mismatch at index ${diffIndex}\n` +
        `SDK length: ${sdkBytes.length}, Rust length: ${rustBytes.length}\n` +
        `SDK bytes:  ${sdkContext}\n` +
        `Rust bytes: ${rustContext}\n` +
        `SDK byte at ${diffIndex}: 0x${sdkBytes[diffIndex]?.toString(16).padStart(2, '0')}\n` +
        `Rust byte at ${diffIndex}: 0x${rustBytes[diffIndex]?.toString(16).padStart(2, '0')}`
      );
    }

    expect(sdkBase64).toBe(rustOutput.message_base64);
    expect(sdkBytes.length).toBe(rustOutput.message_bytes);
  }, 30000);

  it('g2_memo: byte-for-byte equality on transfer + Memo', async () => {
    const goldenPath = join(goldensDir, 'g2_memo.json');
    const golden = await readGolden(goldenPath);

    const userIxs = buildUserInstructions(golden);

    const messageV0 = buildMessageV0Parity({
      payer: new PublicKey(golden.payer),
      recentBlockhash: golden.recentBlockhash,
      cuLimit: golden.cuLimit,
      cuPriceMicrolamports: golden.cuPriceMicrolamports,
      userIxs,
      lookups: golden.alts.lookups.map(l => ({
        tablePubkey: new PublicKey(l.tablePubkey),
        writableIndexes: l.writableIndexes,
        readonlyIndexes: l.readonlyIndexes,
      })),
    });

    const sdkBytes = messageV0.serialize();
    const sdkBase64 = Buffer.from(sdkBytes).toString('base64');

    const rustOutput = await runRustGolden(goldenPath);
    const rustBytes = Buffer.from(rustOutput.message_base64, 'base64');

    if (sdkBase64 !== rustOutput.message_base64) {
      const diffIndex = findFirstDiff(sdkBytes, rustBytes) ?? 0;
      const sdkContext = formatBytesAround(sdkBytes, diffIndex);
      const rustContext = formatBytesAround(rustBytes, diffIndex);
      throw new Error(
        `Byte mismatch at index ${diffIndex}\n` +
        `SDK length: ${sdkBytes.length}, Rust length: ${rustBytes.length}\n` +
        `SDK bytes:  ${sdkContext}\n` +
        `Rust bytes: ${rustContext}\n` +
        `SDK byte at ${diffIndex}: 0x${sdkBytes[diffIndex]?.toString(16).padStart(2, '0')}\n` +
        `Rust byte at ${diffIndex}: 0x${rustBytes[diffIndex]?.toString(16).padStart(2, '0')}`
      );
    }

    expect(sdkBase64).toBe(rustOutput.message_base64);
    expect(sdkBytes.length).toBe(rustOutput.message_bytes);
  }, 30000);

  it('g3_alt: byte-for-byte equality with Address Lookup Table', async () => {
    const goldenPath = join(goldensDir, 'g3_alt.json');
    const golden = await readGolden(goldenPath);

    const userIxs = buildUserInstructions(golden);

    const messageV0 = buildMessageV0Parity({
      payer: new PublicKey(golden.payer),
      recentBlockhash: golden.recentBlockhash,
      cuLimit: golden.cuLimit,
      cuPriceMicrolamports: golden.cuPriceMicrolamports,
      userIxs,
      lookups: golden.alts.lookups.map(l => ({
        tablePubkey: new PublicKey(l.tablePubkey),
        writableIndexes: l.writableIndexes,
        readonlyIndexes: l.readonlyIndexes,
      })),
    });

    const sdkBytes = messageV0.serialize();
    const sdkBase64 = Buffer.from(sdkBytes).toString('base64');

    const rustOutput = await runRustGolden(goldenPath);
    const rustBytes = Buffer.from(rustOutput.message_base64, 'base64');

    if (sdkBase64 !== rustOutput.message_base64) {
      const diffIndex = findFirstDiff(sdkBytes, rustBytes) ?? 0;
      const sdkContext = formatBytesAround(sdkBytes, diffIndex);
      const rustContext = formatBytesAround(rustBytes, diffIndex);
      throw new Error(
        `ALT parity mismatch at index ${diffIndex}\n` +
        `SDK length: ${sdkBytes.length}, Rust length: ${rustBytes.length}\n` +
        `SDK bytes:  ${sdkContext}\n` +
        `Rust bytes: ${rustContext}\n` +
        `SDK byte at ${diffIndex}: 0x${sdkBytes[diffIndex]?.toString(16).padStart(2, '0')}\n` +
        `Rust byte at ${diffIndex}: 0x${rustBytes[diffIndex]?.toString(16).padStart(2, '0')}`
      );
    }

    expect(sdkBase64).toBe(rustOutput.message_base64);
    expect(sdkBytes.length).toBe(rustOutput.message_bytes);
  }, 30000);
});
