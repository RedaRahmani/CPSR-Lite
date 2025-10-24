/**
 * Magic Router transaction submitter
 * @module submit/router
 */

import {
  Signer,
  VersionedTransaction,
} from '@solana/web3.js';
import { SubmitLane, SubmitOptions, SubmitResult } from './types.js';
import {
  NetworkError,
  RouterJsonRpcError,
} from '../errors.js';

/**
 * Magic Router submitter
 * 
 * Submits transactions via Magic Router endpoint with optional preflight skipping
 */
export class RouterSubmitter {
  constructor(
    private readonly routerUrl: string,
    private readonly apiKey?: string
  ) {}

  async submit(
    transaction: VersionedTransaction,
    signers: Signer[],
    options: SubmitOptions = {}
  ): Promise<SubmitResult> {
    const {
      skipPreflight = true,
      maxRetries = 3,
    } = options;

    // Sign the transaction
    transaction.sign(signers);

    // Serialize
    const serialized = transaction.serialize();
    const base64Tx = Buffer.from(serialized).toString('base64');

    let attempts = 0;
    let lastError: Error | null = null;

    while (attempts < maxRetries) {
      attempts++;

      try {
        const response = await this.sendJsonRpc('sendTransaction', [
          base64Tx,
          {
            encoding: 'base64',
            skipPreflight,
            preflightCommitment: 'processed',
          },
        ]);

        if (response.error) {
          throw new RouterJsonRpcError(
            response.error.code,
            response.error.message
          );
        }

        if (!response.result) {
          throw new Error('Router returned no signature');
        }

        return {
          signature: response.result as string,
          lane: SubmitLane.Router,
          preflightSkipped: skipPreflight,
          attempts,
        };
      } catch (error) {
        lastError = error instanceof Error ? error : new Error(String(error));

        const retriable = this.isRetriable(lastError);
        if (!retriable || attempts >= maxRetries) {
          throw lastError;
        }

        // Backoff before retry
        const delay = Math.min(1000 * Math.pow(2, attempts - 1), 5000);
        await new Promise((resolve) => setTimeout(resolve, delay));
      }
    }

    throw lastError ?? new Error('Unknown error');
  }

  private async sendJsonRpc(
    method: string,
    params: unknown[]
  ): Promise<{ result?: unknown; error?: { code: number; message: string } }> {
    const headers: Record<string, string> = {
      'Content-Type': 'application/json',
    };

    if (this.apiKey) {
      headers['x-api-key'] = this.apiKey;
    }

    const body = JSON.stringify({
      jsonrpc: '2.0',
      id: Date.now(),
      method,
      params,
    });

    try {
      const response = await fetch(this.routerUrl, {
        method: 'POST',
        headers,
        body,
      });

      if (!response.ok) {
        throw new NetworkError(
          `Router HTTP error: ${response.status} ${response.statusText}`
        );
      }

      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const json = await response.json() as any;
      return json;
    } catch (error) {
      if (error instanceof Error) {
        throw new NetworkError(`Router request failed: ${error.message}`, error);
      }
      throw error;
    }
  }

  private isRetriable(error: Error): boolean {
    const msg = error.message.toLowerCase();
    return (
      msg.includes('timeout') ||
      msg.includes('429') ||
      msg.includes('rate limit') ||
      msg.includes('network') ||
      msg.includes('connection')
    );
  }
}
