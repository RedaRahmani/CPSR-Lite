/**
 * Router test: OK path with skipPreflight
 */

import { Keypair, SystemProgram, VersionedTransaction } from '@solana/web3.js';
import { RouterSubmitter } from '../../../src/submit/router.js';
import { MockRouterServer } from '../../mocks/router-server.js';
import { buildTransaction } from '../../../src/tx/builder.js';
import { UserIntent, AccessKind } from '../../../src/intents/types.js';

describe('Router: OK path', () => {
  let mockServer: MockRouterServer;
  let submitter: RouterSubmitter;

  beforeAll(async () => {
    mockServer = new MockRouterServer(8765);
    await mockServer.start();
    submitter = new RouterSubmitter(mockServer.getUrl());
  });

  afterAll(async () => {
    await mockServer.stop();
  });

  it('should send transaction with skipPreflight=true and return signature', async () => {
    mockServer.setBehavior({ mode: 'ok' });

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
      maxRetries: 1,
    });

    expect(result.signature).toBeDefined();
    expect(typeof result.signature).toBe('string');
    expect(result.lane).toBe('Router');
    expect(result.preflightSkipped).toBe(true);
    expect(result.attempts).toBe(1);
  });

  it('should work without API key', async () => {
    mockServer.setBehavior({ mode: 'ok' });

    const submitterNoKey = new RouterSubmitter(mockServer.getUrl());
    const payer = Keypair.generate();
    const recipient = Keypair.generate();

    const transferIx = SystemProgram.transfer({
      fromPubkey: payer.publicKey,
      toPubkey: recipient.publicKey,
      lamports: 500,
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

    const result = await submitterNoKey.submit(tx, [payer], {
      skipPreflight: true,
    });

    expect(result.signature).toBeDefined();
  });
});
