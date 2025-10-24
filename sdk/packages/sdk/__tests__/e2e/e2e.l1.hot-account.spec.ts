/**
 * E2E test: Hot account contention - multiple transfers to same recipient
 * 
 * Tests retry/backoff behavior when multiple transactions contend on the same account
 */

import {
  Connection,
  Keypair,
  LAMPORTS_PER_SOL,
  SystemProgram,
} from '@solana/web3.js';
import { buildTransaction } from '../../src/tx/builder.js';
import { BasicPolicy } from '../../src/tx/fees.js';
import { L1Submitter } from '../../src/submit/l1.js';
import { UserIntent, AccessKind } from '../../src/intents/types.js';

const RPC_URL = process.env['SOLANA_RPC_URL'] || 'http://localhost:8899';

describe('E2E: Hot Account Contention', () => {
  let connection: Connection;
  let payer: Keypair;
  let hotRecipient: Keypair;
  let submitter: L1Submitter;
  let feePolicy: BasicPolicy;

  beforeAll(async () => {
    connection = new Connection(RPC_URL, 'confirmed');
    payer = Keypair.generate();
    hotRecipient = Keypair.generate();
    submitter = new L1Submitter(connection);
    feePolicy = new BasicPolicy();

    // Airdrop to payer
    const airdropSig = await connection.requestAirdrop(
      payer.publicKey,
      5 * LAMPORTS_PER_SOL
    );
    await connection.confirmTransaction(airdropSig, 'confirmed');

    const balance = await connection.getBalance(payer.publicKey);
    expect(balance).toBeGreaterThanOrEqual(5 * LAMPORTS_PER_SOL);
  }, 60000);

  it('should handle multiple transfers to same recipient with retry', async () => {
    const transferAmount = 0.01 * LAMPORTS_PER_SOL;
    const numTransfers = 5;
    const signatures: string[] = [];

    // Submit multiple transfers rapidly to same recipient (hot account)
    for (let i = 0; i < numTransfers; i++) {
      const transferIx = SystemProgram.transfer({
        fromPubkey: payer.publicKey,
        toPubkey: hotRecipient.publicKey,
        lamports: transferAmount,
      });

      const intent: UserIntent = {
        actor: payer.publicKey,
        ix: transferIx,
        priority: 0,
        accesses: [
          { pubkey: payer.publicKey, access: AccessKind.Writable },
          { pubkey: hotRecipient.publicKey, access: AccessKind.Writable },
        ],
      };

      // Get fresh blockhash for each transaction
      const { blockhash } = await connection.getLatestBlockhash('confirmed');
      const feePlan = await feePolicy.suggest();

      const tx = await buildTransaction({
        intents: [intent],
        feePlan,
        recentBlockhash: blockhash,
        payer: payer.publicKey,
      });

      // Submit with retry enabled
      const result = await submitter.submit(tx, [payer], {
        skipPreflight: false,
        maxRetries: 5, // Allow retries for contention
      });

      expect(result.signature).toBeDefined();
      expect(result.attempts).toBeGreaterThan(0);
      signatures.push(result.signature);

      // Small delay between submissions to avoid overwhelming validator
      await new Promise((resolve) => setTimeout(resolve, 100));
    }

    // Verify all transactions eventually succeeded
    expect(signatures.length).toBe(numTransfers);

    // Confirm all transactions
    for (const sig of signatures) {
      await connection.confirmTransaction(sig, 'confirmed');
    }

    // Verify final recipient balance
    const finalBalance = await connection.getBalance(hotRecipient.publicKey);
    const expectedBalance = transferAmount * numTransfers;
    expect(finalBalance).toBe(expectedBalance);
  }, 90000);

  it('should demonstrate backoff with retry attempts', async () => {
    const transferAmount = 0.01 * LAMPORTS_PER_SOL;
    
    const transferIx = SystemProgram.transfer({
      fromPubkey: payer.publicKey,
      toPubkey: hotRecipient.publicKey,
      lamports: transferAmount,
    });

    const intent: UserIntent = {
      actor: payer.publicKey,
      ix: transferIx,
      priority: 0,
      accesses: [
        { pubkey: payer.publicKey, access: AccessKind.Writable },
        { pubkey: hotRecipient.publicKey, access: AccessKind.Writable },
      ],
    };

    const { blockhash } = await connection.getLatestBlockhash('confirmed');
    const feePlan = await feePolicy.suggest();

    const tx = await buildTransaction({
      intents: [intent],
      feePlan,
      recentBlockhash: blockhash,
      payer: payer.publicKey,
    });

    const startTime = Date.now();

    const result = await submitter.submit(tx, [payer], {
      skipPreflight: false,
      maxRetries: 5,
    });

    const endTime = Date.now();
    const duration = endTime - startTime;

    expect(result.signature).toBeDefined();
    expect(result.attempts).toBeGreaterThan(0);
    expect(result.attempts).toBeLessThanOrEqual(5);

    // If multiple attempts, some backoff should have occurred
    if (result.attempts > 1) {
      expect(duration).toBeGreaterThan(100); // At least some delay
    }

    await connection.confirmTransaction(result.signature, 'confirmed');
  }, 60000);
});
