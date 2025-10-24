/**
 * L1 (direct RPC) transaction submitter
 * @module submit/l1
 */

import {
  Connection,
  SendOptions,
  Signer,
  VersionedTransaction,
} from '@solana/web3.js';
import { SubmitLane, SubmitOptions, SubmitResult } from './types.js';
import {
  BlockhashStaleError,
  PreflightFailedError,
  RpcRetriableError,
} from '../errors.js';

/**
 * Submit a transaction via direct L1 RPC
 * 
 * Flow:
 * 1. Optionally simulate (if skipPreflight=false)
 * 2. Sign transaction
 * 3. Send via sendTransaction
 * 4. Retry on retriable errors
 */
export class L1Submitter {
  constructor(private readonly connection: Connection) {}

  async submit(
    transaction: VersionedTransaction,
    signers: Signer[],
    options: SubmitOptions = {}
  ): Promise<SubmitResult> {
    const {
      skipPreflight = false,
      maxRetries = 3,
    } = options;

    let attempts = 0;
    let lastError: Error | null = null;

    while (attempts < maxRetries) {
      attempts++;

      try {
        // Simulate if preflight not skipped
        if (!skipPreflight) {
          const simulation = await this.connection.simulateTransaction(
            transaction,
            {
              sigVerify: false,
              replaceRecentBlockhash: false,
            }
          );

          if (simulation.value.err) {
            throw new PreflightFailedError(
              `Simulation failed: ${JSON.stringify(simulation.value.err)}`,
              simulation.value.logs ?? undefined
            );
          }
        }

        // Sign the transaction
        transaction.sign(signers);

        // Send
        const sendOptions: SendOptions = {
          skipPreflight,
          preflightCommitment: 'processed',
        };

        const signature = await this.connection.sendTransaction(
          transaction,
          sendOptions
        );

        return {
          signature,
          lane: SubmitLane.L1,
          preflightSkipped: skipPreflight,
          attempts,
        };
      } catch (error) {
        lastError = error instanceof Error ? error : new Error(String(error));

        // Check if retriable
        const retriable = this.isRetriable(lastError);
        if (!retriable || attempts >= maxRetries) {
          throw this.wrapError(lastError);
        }

        // Backoff before retry
        const delay = Math.min(1000 * Math.pow(2, attempts - 1), 5000);
        await new Promise((resolve) => setTimeout(resolve, delay));
      }
    }

    throw this.wrapError(lastError ?? new Error('Unknown error'));
  }

  private isRetriable(error: Error): boolean {
    const msg = error.message.toLowerCase();
    return (
      msg.includes('timeout') ||
      msg.includes('429') ||
      msg.includes('rate limit') ||
      msg.includes('too many requests') ||
      msg.includes('network') ||
      msg.includes('connection')
    );
  }

  private wrapError(error: Error): Error {
    const msg = error.message.toLowerCase();

    if (msg.includes('blockhash') && msg.includes('not found')) {
      return new BlockhashStaleError(error.message);
    }

    if (msg.includes('429') || msg.includes('rate limit')) {
      return new RpcRetriableError(error.message, 429);
    }

    return error;
  }
}
