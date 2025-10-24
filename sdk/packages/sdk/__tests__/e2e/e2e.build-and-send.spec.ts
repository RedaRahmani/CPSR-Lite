/**
 * E2E tests with solana-test-validator
 */

import {
  Connection,
  Keypair,
  LAMPORTS_PER_SOL,
  PublicKey,
} from '@solana/web3.js';
import { CpsrClient } from '../../src/client';
import { transferIntent, memoIntent } from '../../src/intents/builders';
import { SubmitLane } from '../../src/submit/types';
import { BasicPolicy } from '../../src/tx/fees';
import { exec } from 'child_process';
import { promisify } from 'util';

const execAsync = promisify(exec);

describe('E2E Tests with solana-test-validator', () => {
  let connection: Connection;
  let payer: Keypair;
  let validatorProcess: ReturnType<typeof exec> | null = null;

  beforeAll(async () => {
    // Check if solana-test-validator is available
    try {
      await execAsync('which solana-test-validator');
    } catch (error) {
      console.warn('solana-test-validator not found, skipping E2E tests');
      return;
    }

    // Start validator
    validatorProcess = exec('solana-test-validator --reset --quiet');
    
    // Wait for validator to be ready
    await new Promise((resolve) => setTimeout(resolve, 5000));

    connection = new Connection('http://localhost:8899', 'confirmed');
    payer = Keypair.generate();

    // Airdrop SOL to payer
    const signature = await connection.requestAirdrop(
      payer.publicKey,
      2 * LAMPORTS_PER_SOL
    );
    await connection.confirmTransaction(signature);
  }, 30000);

  afterAll(async () => {
    if (validatorProcess) {
      validatorProcess.kill();
      // Give it time to shut down
      await new Promise((resolve) => setTimeout(resolve, 2000));
    }
  });

  it('should build and submit a simple transfer via L1', async () => {
    if (!connection || !payer) {
      console.log('Skipping test: validator not available');
      return;
    }

    const recipient = Keypair.generate().publicKey;
    const client = new CpsrClient({
      connection,
      payer,
      feePolicy: new BasicPolicy(1_400_000, 0, 5_000),
    });

    const intent = transferIntent({
      from: payer.publicKey,
      to: recipient,
      lamports: 100_000,
    });

    const result = await client.buildAndSubmit({
      intents: [intent],
      cuPrice: 100,
      cuLimit: 200_000,
      submitOptions: {
        lane: SubmitLane.L1,
        skipPreflight: false,
      },
    });

    expect(result.signature).toBeDefined();
    expect(result.lane).toBe(SubmitLane.L1);
    expect(result.attempts).toBeGreaterThan(0);

    // Verify recipient balance
    const balance = await connection.getBalance(recipient);
    expect(balance).toBe(100_000);
  }, 30000);

  it('should handle memo intents', async () => {
    if (!connection || !payer) {
      console.log('Skipping test: validator not available');
      return;
    }

    const client = new CpsrClient({
      connection,
      payer,
    });

    const intent = memoIntent({
      payer: payer.publicKey,
      message: 'CPSR test memo',
    });

    const result = await client.buildAndSubmit({
      intents: [intent],
      cuPrice: 100,
      cuLimit: 200_000,
      submitOptions: {
        lane: SubmitLane.L1,
      },
    });

    expect(result.signature).toBeDefined();
  }, 30000);

  it('should handle hot account scenario with retries', async () => {
    if (!connection || !payer) {
      console.log('Skipping test: validator not available');
      return;
    }

    const hotAccount = Keypair.generate().publicKey;
    const client = new CpsrClient({
      connection,
      payer,
    });

    // Submit multiple transfers to the same account rapidly
    const promises = [];
    for (let i = 0; i < 3; i++) {
      const intent = transferIntent({
        from: payer.publicKey,
        to: hotAccount,
        lamports: 10_000 * (i + 1),
      });

      promises.push(
        client.buildAndSubmit({
          intents: [intent],
          cuPrice: 100,
          cuLimit: 200_000,
          submitOptions: {
            lane: SubmitLane.L1,
            maxRetries: 5,
          },
        })
      );
    }

    const results = await Promise.all(promises);
    expect(results).toHaveLength(3);
    
    // All should succeed
    results.forEach((result) => {
      expect(result.signature).toBeDefined();
    });

    // Final balance should be sum of all transfers
    const balance = await connection.getBalance(hotAccount);
    expect(balance).toBe(10_000 + 20_000 + 30_000);
  }, 60000);

  it('should tighten CU limit via simulation', async () => {
    if (!connection || !payer) {
      console.log('Skipping test: validator not available');
      return;
    }

    const recipient = Keypair.generate().publicKey;
    const client = new CpsrClient({
      connection,
      payer,
    });

    const intent = transferIntent({
      from: payer.publicKey,
      to: recipient,
      lamports: 50_000,
    });

    // Build with tightening enabled
    const tx = await client.build({
      intents: [intent],
      cuLimit: 1_000_000, // High initial limit
      cuPrice: 100,
      tighten: true,
    });

    expect(tx).toBeDefined();
    
    // Submit
    const result = await client.submit(tx, {
      lane: SubmitLane.L1,
    });

    expect(result.signature).toBeDefined();
  }, 30000);
});
