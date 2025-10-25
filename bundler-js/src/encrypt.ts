/**
 * Encryption helpers using official Arcium SDK
 * Uses x25519 + RescueCipher with proper HKDF + Rescue-Prime domain
 */

import { Connection, PublicKey } from '@solana/web3.js';
import { x25519, RescueCipher } from '@arcium-hq/client';
import { randomBytes } from 'crypto';
import { AnchorProvider } from '@coral-xyz/anchor';
import { PlannerInput, PlanOutput, packPlannerInput, unpackPlanOutput } from './types';

/**
 * Generate ephemeral x25519 keypair using Arcium SDK
 */
export function makeEphemeralKeypair(): {
  publicKey: Uint8Array;
  secretKey: Uint8Array;
} {
  const secretKey = x25519.utils.randomSecretKey();
  const publicKey = x25519.getPublicKey(secretKey);
  return { publicKey, secretKey };
}

/**
 * Derive shared secret using x25519 ECDH (Arcium SDK)
 */
export function deriveSharedSecret(
  ephemeralPrivateKey: Uint8Array,
  mxePublicKey: Uint8Array
): Uint8Array {
  return x25519.getSharedSecret(ephemeralPrivateKey, mxePublicKey);
}

/**
 * Fetch MXE public key using official Arcium SDK helper or env override
 * This handles account layout changes across versions properly
 */
export async function getMXEPublicKeyWithRetry(
  connection: Connection,
  programId: PublicKey,
  maxRetries: number = 3
): Promise<Uint8Array> {
  // 1) If operator provides the MXE pubkey directly (hex/base58), use it
  const override = process.env.ARCIUM_MXE_PUBKEY;
  if (override) {
    try {
      // Accept base58 or hex
      const bs58 = await import('bs58');
      const isHex = /^([0-9a-f]{2})+$/i.test(override);
      const bytes = isHex 
        ? Uint8Array.from(override.match(/.{1,2}/g)!.map(h => parseInt(h, 16)))
        : bs58.default.decode(override);
      if (bytes.length !== 32) throw new Error('Bad length');
      return bytes;
    } catch (e) {
      throw new Error(`ARCIUM_MXE_PUBKEY is invalid: ${e}`);
    }
  }

  // 2) If SDK exposes a helper, dynamically use it (no hard dependency on name)
  const provider = new AnchorProvider(
    connection,
    {} as any,
    { commitment: 'confirmed' }
  );

  const mod: any = await import('@arcium-hq/client');
  const helper = mod.getMXEPublicKey || mod.getMXEPublicKeyWithRetry || mod.fetchMXEPublicKey;
  if (typeof helper !== 'function') {
    throw new Error(
      'MXE public key helper not available in @arcium-hq/client. ' +
      'Set ARCIUM_MXE_PUBKEY (base58/hex) or upgrade the SDK.'
    );
  }
  
  for (let attempt = 0; attempt < maxRetries; attempt++) {
    try {
      const key: Uint8Array = await helper(provider, programId);
      return key;
    } catch (err) {
      if (attempt === maxRetries - 1) throw err;
      await new Promise(r => setTimeout(r, 500 * (attempt + 1)));
    }
  }
  throw new Error('Failed to fetch MXE public key after retries.');
}

/**
 * Encrypt PlannerInput using official Arcium RescueCipher
 * Returns number[][] to match SDK type expectations
 * This ensures compatibility with MXE decryption
 */
export function encryptPlannerInput(
  input: PlannerInput,
  sharedSecret: Uint8Array,
  nonceBytes: Uint8Array
): number[][] {
  // Pack input into field elements (now returns number[])
  const fields = packPlannerInput(input);
  
  // Convert number[] to bigint[] for RescueCipher
  const fieldsBigInt = fields.map(f => BigInt(f));
  
  // Use official RescueCipher with proper HKDF + Rescue-Prime
  const cipher = new RescueCipher(sharedSecret);
  // SDK expects Uint8Array for nonce, returns number[][]
  return cipher.encrypt(fieldsBigInt, nonceBytes);
}

/**
 * Decrypt PlanOutput using official Arcium RescueCipher
 */
export function decryptPlanOutput(
  ciphertexts: number[][],
  sharedSecret: Uint8Array,
  nonceBytes: Uint8Array
): PlanOutput {
  // Use official RescueCipher for decryption
  const cipher = new RescueCipher(sharedSecret);
  const fields = cipher.decrypt(ciphertexts, nonceBytes);
  
  return unpackPlanOutput(fields);
}

/**
 * Generate random 128-bit nonce using Node's crypto
 */
export function generateNonce(): bigint {
  const bytes = new Uint8Array(randomBytes(16));
  // Construct 128-bit nonce as little-endian (byte 0 = LSB)
  let nonce = 0n;
  for (let i = 0; i < 16; i++) {
    nonce |= BigInt(bytes[i]) << BigInt(i * 8);
  }
  return nonce;
}

/**
 * Convert nonce bigint to 16-byte array
 */
export function nonceToBytes(nonce: bigint): Uint8Array {
  const bytes = new Uint8Array(16);
  // Convert nonce to 16-byte array as little-endian (byte 0 = LSB)
  for (let i = 0; i < 16; i++) {
    bytes[i] = Number((nonce >> BigInt(i * 8)) & 0xffn);
  }
  return bytes;
}

/**
 * Hash a value for secure logging (first 8 hex chars of SHA-256-like hash)
 * Simple implementation using XOR folding
 */
export function hashForLog(data: Uint8Array): string {
  let hash = 0;
  for (let i = 0; i < data.length; i++) {
    hash = ((hash << 5) - hash) + data[i];
    hash = hash & hash; // Convert to 32bit integer
  }
  return Math.abs(hash).toString(16).slice(0, 8).padStart(8, '0');
}
