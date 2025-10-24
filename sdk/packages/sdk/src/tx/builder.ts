/**
 * Transaction builder with compute budget and ALT support
 * @module tx/builder
 */

import {
  ComputeBudgetProgram,
  Connection,
  PublicKey,
  TransactionMessage,
  VersionedTransaction,
} from '@solana/web3.js';
import { UserIntent } from '../intents/types.js';
import { FeePlan } from './fees.js';
import { AddressLookupSource, NoAltSource } from './alt.js';
import { TransactionTooLargeError } from '../errors.js';

/**
 * Options for building a transaction
 */
export interface BuildTransactionOptions {
  /** Intents to include */
  intents: UserIntent[];
  /** Fee plan (CU limit and price) */
  feePlan: FeePlan;
  /** Recent blockhash */
  recentBlockhash: string;
  /** Transaction payer */
  payer: PublicKey;
  /** Optional ALT source */
  altSource?: AddressLookupSource;
  /** Maximum message size in bytes (default: 1232) */
  maxMessageSize?: number;
}

/**
 * Build a versioned transaction from intents
 * 
 * This function:
 * 1. Prepends ComputeBudget instructions (setComputeUnitLimit, setComputeUnitPrice)
 * 2. Appends user instructions from intents
 * 3. Resolves ALTs if provided
 * 4. Compiles a v0 message
 * 5. Validates size constraints
 */
export async function buildTransaction(
  options: BuildTransactionOptions
): Promise<VersionedTransaction> {
  const {
    intents,
    feePlan,
    recentBlockhash,
    payer,
    altSource = new NoAltSource(),
    maxMessageSize = 1232,
  } = options;

  if (intents.length === 0) {
    throw new Error('Cannot build transaction with zero intents');
  }

  // 1. Prepend compute budget instructions
  const instructions = [
    ComputeBudgetProgram.setComputeUnitLimit({
      units: feePlan.cuLimit,
    }),
    ComputeBudgetProgram.setComputeUnitPrice({
      microLamports: feePlan.cuPriceMicroLamports,
    }),
  ];

  // 2. Append user instructions
  for (const intent of intents) {
    instructions.push(intent.ix);
  }

  // 3. Resolve ALTs
  const lookupTables = await altSource.resolveTables(payer, intents);

  // 4. Compile v0 message
  const messageV0 = new TransactionMessage({
    payerKey: payer,
    recentBlockhash,
    instructions,
  }).compileToV0Message(lookupTables);

  const tx = new VersionedTransaction(messageV0);

  // 5. Validate size (reserve space for signatures)
  const serialized = tx.serialize();
  
  // Message size check: serialized tx includes placeholder signatures
  // Actual message is smaller, but we check the full serialized size
  if (serialized.length > maxMessageSize) {
    throw new TransactionTooLargeError(
      `Transaction too large: ${serialized.length} bytes (max: ${maxMessageSize})`,
      serialized.length,
      maxMessageSize
    );
  }

  return tx;
}

/**
 * Estimate compute units by simulating a transaction
 */
export async function estimateComputeUnits(
  connection: Connection,
  transaction: VersionedTransaction
): Promise<number> {
  const simulation = await connection.simulateTransaction(transaction, {
    sigVerify: false,
    replaceRecentBlockhash: true,
  });

  if (simulation.value.err) {
    throw new Error(`Simulation failed: ${JSON.stringify(simulation.value.err)}`);
  }

  return simulation.value.unitsConsumed ?? 200_000;
}

/**
 * Build and tighten: simulate, adjust CU limit with safety margin, rebuild
 */
export async function buildAndTighten(
  connection: Connection,
  options: BuildTransactionOptions,
  safetyMarginCU: number = 10_000,
  safetyMarginPercent: number = 20
): Promise<{ tx: VersionedTransaction; usedCU: number; finalPlan: FeePlan }> {
  // First build with initial plan
  const initialTx = await buildTransaction(options);
  
  // Simulate to get actual usage
  const usedCU = await estimateComputeUnits(connection, initialTx);
  
  // Apply safety: used + margin + %
  const withMargin = usedCU + safetyMarginCU;
  const withPercent = Math.ceil(withMargin * (1 + safetyMarginPercent / 100));
  const tightenedLimit = Math.min(withPercent, options.feePlan.cuLimit);
  
  const finalPlan: FeePlan = {
    cuLimit: tightenedLimit,
    cuPriceMicroLamports: options.feePlan.cuPriceMicroLamports,
  };
  
  // Rebuild with tightened limit
  const finalTx = await buildTransaction({
    ...options,
    feePlan: finalPlan,
  });
  
  return { tx: finalTx, usedCU, finalPlan };
}
