/**
 * Address Lookup Table (ALT) resolution and management
 * @module tx/alt
 */

import {
  AddressLookupTableAccount,
  PublicKey,
} from '@solana/web3.js';
import { UserIntent } from '../intents/types.js';

/**
 * Interface for providing address lookup tables
 */
export interface AddressLookupSource {
  /**
   * Resolve ALTs for a set of intents
   * @param payer - Transaction payer
   * @param intents - Intents to analyze for address offloading
   * @returns Array of lookup table accounts to include
   */
  resolveTables(
    payer: PublicKey,
    intents: UserIntent[]
  ): Promise<AddressLookupTableAccount[]>;
}

/**
 * No-op ALT source (returns empty array)
 */
export class NoAltSource implements AddressLookupSource {
  async resolveTables(): Promise<AddressLookupTableAccount[]> {
    return [];
  }
}

/**
 * Static ALT source with pre-configured tables
 */
export class StaticAltSource implements AddressLookupSource {
  constructor(private readonly tables: AddressLookupTableAccount[]) {}

  async resolveTables(): Promise<AddressLookupTableAccount[]> {
    return this.tables;
  }
}

/**
 * ALT resolution statistics
 */
export interface AltStats {
  /** Number of keys offloaded to ALTs */
  keysOffloaded: number;
  /** Number of readonly keys offloaded */
  readonlyOffloaded: number;
  /** Number of writable keys offloaded */
  writableOffloaded: number;
  /** Estimated bytes saved */
  estimatedSavedBytes: number;
}

/**
 * Result of ALT resolution
 */
export interface AltResolution {
  /** Resolved lookup tables */
  tables: AddressLookupTableAccount[];
  /** Statistics about offloading */
  stats: AltStats;
}

/**
 * Helper to compute ALT statistics
 */
export function computeAltStats(
  intents: UserIntent[],
  tables: AddressLookupTableAccount[]
): AltStats {
  const allAddresses = new Set<string>();
  const writableAddresses = new Set<string>();

  for (const intent of intents) {
    for (const access of intent.accesses) {
      const key = access.pubkey.toBase58();
      allAddresses.add(key);
      if (access.access === 'writable') {
        writableAddresses.add(key);
      }
    }
  }

  const lookupAddresses = new Set<string>();
  for (const table of tables) {
    // AddressLookupTableAccount has state.addresses
    const addresses = table.state?.addresses ?? [];
    for (const addr of addresses) {
      lookupAddresses.add(addr.toBase58());
    }
  }

  const offloadedKeys = new Set<string>();
  const offloadedWritable = new Set<string>();
  
  for (const key of allAddresses) {
    if (lookupAddresses.has(key)) {
      offloadedKeys.add(key);
      if (writableAddresses.has(key)) {
        offloadedWritable.add(key);
      }
    }
  }

  const keysOffloaded = offloadedKeys.size;
  const writableOffloaded = offloadedWritable.size;
  const readonlyOffloaded = keysOffloaded - writableOffloaded;

  // Each offloaded key saves 32 bytes (pubkey) but costs ~1 byte (index)
  const estimatedSavedBytes = keysOffloaded * 31;

  return {
    keysOffloaded,
    readonlyOffloaded,
    writableOffloaded,
    estimatedSavedBytes,
  };
}
