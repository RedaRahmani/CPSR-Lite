/**
 * Unit tests for intent builders
 */

import { Keypair, SystemProgram } from '@solana/web3.js';
import { buildIntent, transferIntent, memoIntent } from '../../src/intents/builders';
import { AccessKind } from '../../src/intents/types';
import { InvalidIntentError } from '../../src/errors';

describe('Intent Builders', () => {
  const payer = Keypair.generate().publicKey;
  const recipient = Keypair.generate().publicKey;

  describe('buildIntent', () => {
    it('should build a valid intent', () => {
      const ix = SystemProgram.transfer({
        fromPubkey: payer,
        toPubkey: recipient,
        lamports: 1_000_000,
      });

      const intent = buildIntent(payer, ix, { priority: 5 });

      expect(intent.actor).toEqual(payer);
      expect(intent.ix).toEqual(ix);
      expect(intent.priority).toBe(5);
      expect(intent.accesses.length).toBeGreaterThan(0);
    });

    it('should throw for missing program ID', () => {
      const ix = {
        programId: null as any,
        keys: [],
        data: Buffer.from([]),
      };

      expect(() => buildIntent(payer, ix)).toThrow(InvalidIntentError);
    });

    it('should derive accesses correctly', () => {
      const ix = SystemProgram.transfer({
        fromPubkey: payer,
        toPubkey: recipient,
        lamports: 1_000_000,
      });

      const intent = buildIntent(payer, ix);

      // Transfer should have payer (writable) and recipient (writable)
      const payerAccess = intent.accesses.find(a => a.pubkey.equals(payer));
      const recipientAccess = intent.accesses.find(a => a.pubkey.equals(recipient));

      expect(payerAccess).toBeDefined();
      expect(recipientAccess).toBeDefined();
      expect(payerAccess?.access).toBe(AccessKind.Writable);
      expect(recipientAccess?.access).toBe(AccessKind.Writable);
    });
  });

  describe('transferIntent', () => {
    it('should build a transfer intent', () => {
      const intent = transferIntent({
        from: payer,
        to: recipient,
        lamports: 1_000_000,
        options: { priority: 1 },
      });

      expect(intent.actor).toEqual(payer);
      expect(intent.priority).toBe(1);
      expect(intent.ix.programId).toEqual(SystemProgram.programId);
    });

    it('should handle bigint lamports', () => {
      const intent = transferIntent({
        from: payer,
        to: recipient,
        lamports: BigInt(1_000_000),
      });

      expect(intent).toBeDefined();
    });
  });

  describe('memoIntent', () => {
    it('should build a memo intent', () => {
      const intent = memoIntent({
        payer,
        message: 'Hello, CPSR!',
      });

      expect(intent.actor).toEqual(payer);
      expect(intent.ix.data.toString('utf-8')).toBe('Hello, CPSR!');
    });
  });
});
