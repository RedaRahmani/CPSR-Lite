/**
 * Router test: Preflight simulation
 */

import { Keypair, SystemProgram } from '@solana/web3.js';
import { RouterSubmitter } from '../../../src/submit/router.js';
import { MockRouterServer } from '../../mocks/router-server.js';
import { buildTransaction } from '../../../src/tx/builder.js';
import { UserIntent, AccessKind } from '../../../src/intents/types.js';
import { RouterJsonRpcError } from '../../../src/errors.js';

describe('Router: Preflight', () => {
  let mockServer: MockRouterServer;
  let submitter: RouterSubmitter;

  beforeAll(async () => {
    mockServer = new MockRouterServer(8766);
    await mockServer.start();
    submitter = new RouterSubmitter(mockServer.getUrl());
  });

  afterAll(async () => {
    await mockServer.stop();
  });

  it('should fail when preflight returns error', async () => {
    mockServer.setBehavior({
      mode: 'preflight-fail',
      errorMessage: 'Transaction simulation failed',
    });

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
        skipPreflight: false,
        maxRetries: 1,
      })
    ).rejects.toThrow(RouterJsonRpcError);
  });

  it('should map Router JSON-RPC errors to typed SDK errors', async () => {
    mockServer.setBehavior({
      mode: 'preflight-fail',
      errorCode: -32002,
      errorMessage: 'Custom router error',
    });

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

    try {
      await submitter.submit(tx, [payer], {
        skipPreflight: false,
        maxRetries: 1,
      });
      fail('Should have thrown RouterJsonRpcError');
    } catch (error) {
      expect(error).toBeInstanceOf(RouterJsonRpcError);
      const rpcError = error as RouterJsonRpcError;
      expect(rpcError.code).toBe(-32002);
      expect(rpcError.message).toContain('Custom router error');
    }
  });
});
