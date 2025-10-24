/**
 * Intent builders for common Solana operations
 * @module intents/builders
 */

import {
  PublicKey,
  SystemProgram,
  TransactionInstruction,
} from '@solana/web3.js';
import { AccessKind, AccountAccess, IntentOptions, UserIntent } from './types.js';
import { InvalidIntentError } from '../errors.js';

/**
 * Derive account accesses from a transaction instruction
 */
function deriveAccesses(ix: TransactionInstruction): AccountAccess[] {
  return ix.keys.map((meta) => ({
    pubkey: meta.pubkey,
    access: meta.isWritable ? AccessKind.Writable : AccessKind.Readonly,
  }));
}

/**
 * Build a UserIntent from an instruction and actor
 */
export function buildIntent(
  actor: PublicKey,
  ix: TransactionInstruction,
  options: IntentOptions = {}
): UserIntent {
  // Skip the check for default program ID since SystemProgram uses a valid ID
  // The check was too strict for legitimate program IDs
  if (!ix.programId) {
    throw new InvalidIntentError('Instruction must have a program ID');
  }

  return {
    actor,
    ix,
    priority: options.priority ?? 0,
    versions: options.versions,
    accesses: deriveAccesses(ix),
  };
}

/**
 * Build a SOL transfer intent
 */
export function transferIntent(params: {
  from: PublicKey;
  to: PublicKey;
  lamports: number | bigint;
  options?: IntentOptions;
}): UserIntent {
  const { from, to, lamports, options } = params;
  
  const ix = SystemProgram.transfer({
    fromPubkey: from,
    toPubkey: to,
    lamports: typeof lamports === 'bigint' ? Number(lamports) : lamports,
  });

  return buildIntent(from, ix, options);
}

/**
 * Build a memo intent (SPL Memo program)
 */
export function memoIntent(params: {
  payer: PublicKey;
  message: string;
  options?: IntentOptions;
}): UserIntent {
  const { payer, message, options } = params;
  
  // SPL Memo program ID
  const MEMO_PROGRAM_ID = new PublicKey('MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr');
  
  const ix = new TransactionInstruction({
    programId: MEMO_PROGRAM_ID,
    keys: [{ pubkey: payer, isSigner: true, isWritable: false }],
    data: Buffer.from(message, 'utf-8'),
  });

  return buildIntent(payer, ix, options);
}

/**
 * Build a generic intent from a raw instruction
 */
export function customIntent(params: {
  actor: PublicKey;
  instruction: TransactionInstruction;
  options?: IntentOptions;
}): UserIntent {
  return buildIntent(params.actor, params.instruction, params.options);
}
