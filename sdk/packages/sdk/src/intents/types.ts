/**
 * Core types for CPSR-Lite intents
 * @module intents/types
 */

import { PublicKey, TransactionInstruction } from '@solana/web3.js';

/**
 * Access mode for account metadata
 */
export enum AccessKind {
  /** Account is read-only */
  Readonly = 'readonly',
  /** Account is writable */
  Writable = 'writable',
}

/**
 * Account access metadata
 */
export interface AccountAccess {
  /** Public key of the account */
  pubkey: PublicKey;
  /** Access type (readonly or writable) */
  access: AccessKind;
}

/**
 * Version information for an account at capture time
 */
export interface AccountVersion {
  /** Account public key */
  pubkey: PublicKey;
  /** Lamport balance at capture */
  lamports: bigint;
  /** Account data hash (blake3) */
  dataHash: Uint8Array;
  /** Owner program ID */
  owner: PublicKey;
  /** Is executable flag */
  executable: boolean;
  /** Rent epoch */
  rentEpoch: bigint;
}

/**
 * User intent encapsulating an instruction with metadata
 */
export interface UserIntent {
  /** Actor (payer/authority) for this intent */
  actor: PublicKey;
  /** Solana instruction to execute */
  ix: TransactionInstruction;
  /** Priority (higher = executed first in sorting) */
  priority: number;
  /** Optional OCC version constraints */
  versions?: AccountVersion[];
  /** Derived account access list */
  accesses: AccountAccess[];
}

/**
 * Options for building an intent
 */
export interface IntentOptions {
  /** Priority for ordering (default: 0) */
  priority?: number;
  /** OCC version constraints */
  versions?: AccountVersion[];
}
