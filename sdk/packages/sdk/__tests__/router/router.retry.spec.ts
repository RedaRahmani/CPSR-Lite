/**
 * Router test: Retry and backoff
 */

import { Keypair, SystemProgram } from '@solana/web3.js';
import { RouterSubmitter } from '../../../src/submit/router.js';
import { MockRouterServer } from '../../mocks/router-server.js';
import { buildTransaction } from '../../../src/tx/builder.js';
import { UserIntent, AccessKind } from '../../../src/intents/types.js';

describe('Router: Retry and backoff', () => {
  let mockServer: MockRouterServer;
  let submitter: RouterSubmitter;

  beforeAll(async () => {
    mockServer = new MockRouterServer(8767);
    await mockServer.start();
    submitter = new RouterSubmitter(mockServer.getUrl());
  });

  afterAll(async () => {
    await mockServer.stop();
  });

  it('should retry on 429 rate limit and eventually succeed', async () => {
    let attemptCount = 0;

    // Mock: fail first 2 attempts with 429, then succeed
    const originalSetBehavior = mockServer.setBehavior.bind(mockServer);
    mockServer.setBehavior = (behavior) => {
      attemptCount++;
      if (attemptCount <= 2) {
        originalSetBehavior({ mode: 'rate-limit' });
      } else {
        originalSetBehavior({ mode: 'ok' });
      }
    };

    mockServer.setBehavior({ mode: 'rate-limit' });

    const payer = Keypair.generate();
    const recipient = Keypair.generate();

    const transferIx = SystemProgram.transfer({
      fromPubkey: payer.publicKey,
      toPubkey: recipient.publicKey,
      lamports: 1000,
    });

    const intent: UserIntent = {
      actor: payer.publicKey,
      ix: transferIx,
      priority: 0,
      accesses: [
        { pubkey: payer.publicKey, access: AccessKind.Writable },
        { pubkey: recipient.publicKey, access: AccessKind.Writable },
      ],
    };

    const tx = await buildTransaction({
      intents: [intent],
      feePlan: { cuLimit: 200_000, cuPriceMicroLamports: 100 },
      recentBlockhash: '11111111111111111111111111111111',
      payer: payer.publicKey,
    });

    const startTime = Date.now();

    const result = await submitter.submit(tx, [payer], {
      skipPreflight: true,
      maxRetries: 5,
    });

    const duration = Date.now() - startTime;

    expect(result.signature).toBeDefined();
    expect(result.attempts).toBeGreaterThan(1);
    expect(duration).toBeGreaterThan(100); // Backoff should add delay
  }, 15000);

  it('should retry on 5xx server errors', async () => {
    let attemptCount = 0;

    const originalSetBehavior = mockServer.setBehavior.bind(mockServer);
    mockServer.setBehavior = (behavior) => {
      attemptCount++;
      if (attemptCount === 1) {
        originalSetBehavior({ mode: 'server-error', errorCode: 503 });
      } else {
        originalSetBehavior({ mode: 'ok' });
      }
    };

    mockServer.setBehavior({ mode: 'server-error' });

    const payer = Keypair.generate();
    const recipient = Keypair.generate();

    const transferIx = SystemProgram.transfer({
      fromPubkey: payer.publicKey,
      toPubkey: recipient.publicKey,
      lamports: 1000,
    });

    const intent: UserIntent = {
      actor: payer.publicKey,
      ix: transferIx,
      priority: 0,
      accesses: [
        { pubkey: payer.publicKey, access: AccessKind.Writable },
        { pubkey: recipient.publicKey, access: AccessKind.Writable },
      ],
    };

    const tx = await buildTransaction({
      intents: [intent],
      feePlan: { cuLimit: 200_000, cuPriceMicroLamports: 100 },
      recentBlockhash: '11111111111111111111111111111111',
      payer: payer.publicKey,
    });

    const result = await submitter.submit(tx, [payer], {
      skipPreflight: true,
      maxRetries: 3,
    });

    expect(result.signature).toBeDefined();
    expect(result.attempts).toBeGreaterThan(1);
  }, 10000);

  it('should respect maxRetries and fail after exhausting attempts', async () => {
    mockServer.setBehavior({ mode: 'rate-limit' });

    const payer = Keypair.generate();
    const recipient = Keypair.generate();

    const transferIx = SystemProgram.transfer({
      fromPubkey: payer.publicKey,
      toPubkey: recipient.publicKey,
      lamports: 1000,
    });

    const intent: UserIntent = {
      actor: payer.publicKey,
      ix: transferIx,
      priority: 0,
      accesses: [
        { pubkey: payer.publicKey, access: AccessKind.Writable },
        { pubkey: recipient.publicKey, access: AccessKind.Writable },
      ],
    };

    const tx = await buildTransaction({
      intents: [intent],
      feePlan: { cuLimit: 200_000, cuPriceMicroLamports: 100 },
      recentBlockhash: '11111111111111111111111111111111',
      payer: payer.publicKey,
    });

    await expect(
      submitter.submit(tx, [payer], {
        skipPreflight: true,
        maxRetries: 2,
      })
    ).rejects.toThrow();
  }, 10000);
});
