/**
 * Unit tests for transaction builder
 */

import { Connection, Keypair, SystemProgram } from '@solana/web3.js';
import { buildTransaction, buildAndTighten } from '../../src/tx/builder';
import { buildIntent } from '../../src/intents/builders';
import { NoAltSource } from '../../src/tx/alt';

describe('Transaction Builder', () => {
  const payer = Keypair.generate();
  const recipient = Keypair.generate().publicKey;

  it('should build a transaction with compute budget ixs', async () => {
    const transferIx = SystemProgram.transfer({
      fromPubkey: payer.publicKey,
      toPubkey: recipient,
      lamports: 1_000_000,
    });

    const intent = buildIntent(payer.publicKey, transferIx);

    // Use a valid base58 blockhash
    const validBlockhash = '11111111111111111111111111111111';

    const tx = await buildTransaction({
      intents: [intent],
      feePlan: {
        cuLimit: 200_000,
        cuPriceMicroLamports: 1000,
      },
      recentBlockhash: validBlockhash,
      payer: payer.publicKey,
      altSource: new NoAltSource(),
    });

    expect(tx).toBeDefined();
    expect(tx.message).toBeDefined();

    // Message should have 3 instructions: setComputeUnitLimit, setComputeUnitPrice, transfer
    const ixCount = 'compiledInstructions' in tx.message 
      ? tx.message.compiledInstructions.length 
      : 0;
    expect(ixCount).toBe(3);
  });

  it('should throw error for empty intents', async () => {
    await expect(
      buildTransaction({
        intents: [],
        feePlan: {
          cuLimit: 200_000,
          cuPriceMicroLamports: 1000,
        },
        recentBlockhash: 'DummyBlockhash11111111111111111111111111111',
        payer: payer.publicKey,
      })
    ).rejects.toThrow('Cannot build transaction with zero intents');
  });

  it('should enforce size limits', async () => {
    // Create a large transaction
    const intents = [];
    for (let i = 0; i < 50; i++) {
      const ix = SystemProgram.transfer({
        fromPubkey: payer.publicKey,
        toPubkey: Keypair.generate().publicKey,
        lamports: i + 1,
      });
      intents.push(buildIntent(payer.publicKey, ix));
    }

    const validBlockhash = '11111111111111111111111111111111';

    await expect(
      buildTransaction({
        intents,
        feePlan: {
          cuLimit: 200_000,
          cuPriceMicroLamports: 1000,
        },
        recentBlockhash: validBlockhash,
        payer: payer.publicKey,
        maxMessageSize: 500, // Very small limit
      })
    ).rejects.toThrow(); // Just check that it throws
  });
});

describe('buildAndTighten', () => {
  it('should simulate and tighten CU limit', async () => {
    const mockConnection = {
      simulateTransaction: () => Promise.resolve({
        value: {
          err: null,
          unitsConsumed: 50_000,
          logs: [],
        },
      }),
    } as unknown as Connection;

    const payer = Keypair.generate();
    const recipient = Keypair.generate().publicKey;

    const transferIx = SystemProgram.transfer({
      fromPubkey: payer.publicKey,
      toPubkey: recipient,
      lamports: 1_000_000,
    });

    const intent = buildIntent(payer.publicKey, transferIx);
    const validBlockhash = '11111111111111111111111111111111';

    const result = await buildAndTighten(
      mockConnection,
      {
        intents: [intent],
        feePlan: {
          cuLimit: 200_000,
          cuPriceMicroLamports: 1000,
        },
        recentBlockhash: validBlockhash,
        payer: payer.publicKey,
      },
      10_000, // safety margin
      20 // safety percent
    );

    expect(result.usedCU).toBe(50_000);
    // (50_000 + 10_000) * 1.2 = 72_000
    expect(result.finalPlan.cuLimit).toBe(72_000);
  });
});
