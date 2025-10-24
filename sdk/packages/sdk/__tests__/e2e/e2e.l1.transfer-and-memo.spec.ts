/**
 * E2E test: Transfer and Memo via L1 submitter with solana-test-validator
 */

import {
  Connection,
  Keypair,
  LAMPORTS_PER_SOL,
  PublicKey,
  SystemProgram,
  TransactionInstruction,
} from '@solana/web3.js';
import { buildTransaction } from '../../src/tx/builder.js';
import { BasicPolicy } from '../../src/tx/fees.js';
import { L1Submitter } from '../../src/submit/l1.js';
import { UserIntent, AccessKind } from '../../src/intents/types.js';

const RPC_URL = process.env['SOLANA_RPC_URL'] || 'http://localhost:8899';

describe('E2E: L1 Transfer and Memo', () => {
  let connection: Connection;
  let payer: Keypair;
  let recipient: Keypair;
  let submitter: L1Submitter;
  let feePolicy: BasicPolicy;

  beforeAll(async () => {
    connection = new Connection(RPC_URL, 'confirmed');
    payer = Keypair.generate();
    recipient = Keypair.generate();
    submitter = new L1Submitter(connection);
    feePolicy = new BasicPolicy();

    // Airdrop to payer
    const airdropSig = await connection.requestAirdrop(
      payer.publicKey,
      2 * LAMPORTS_PER_SOL
    );
    await connection.confirmTransaction(airdropSig, 'confirmed');

    // Verify airdrop landed
    const balance = await connection.getBalance(payer.publicKey);
    expect(balance).toBeGreaterThanOrEqual(2 * LAMPORTS_PER_SOL);
  }, 60000);

  it('should transfer SOL from payer to recipient', async () => {
    const transferAmount = 0.1 * LAMPORTS_PER_SOL;

    // Build transfer instruction
    const transferIx = SystemProgram.transfer({
      fromPubkey: payer.publicKey,
      toPubkey: recipient.publicKey,
      lamports: transferAmount,
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

    // Get recent blockhash
    const { blockhash } = await connection.getLatestBlockhash('confirmed');

    // Build transaction
    const feePlan = await feePolicy.suggest();
    const tx = await buildTransaction({
      intents: [intent],
      feePlan,
      recentBlockhash: blockhash,
      payer: payer.publicKey,
    });

    // Submit via L1
    const result = await submitter.submit(tx, [payer], {
      skipPreflight: false,
      maxRetries: 3,
    });

    expect(result.signature).toBeDefined();
    expect(result.lane).toBe('L1');

    // Confirm transaction
    await connection.confirmTransaction(result.signature, 'confirmed');

    // Verify recipient balance
    const recipientBalance = await connection.getBalance(recipient.publicKey);
    expect(recipientBalance).toBe(transferAmount);
  }, 30000);

  it('should send transfer + memo in one transaction', async () => {
    const transferAmount = 0.05 * LAMPORTS_PER_SOL;
    const memoText = 'E2E test memo';

    // Transfer instruction
    const transferIx = SystemProgram.transfer({
      fromPubkey: payer.publicKey,
      toPubkey: recipient.publicKey,
      lamports: transferAmount,
    });

    // Memo instruction (program ID for Memo program)
    const memoProgramId = new PublicKey(
      'MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr'
    );
    const memoIx = new TransactionInstruction({
      keys: [],
      programId: memoProgramId,
      data: Buffer.from(new TextEncoder().encode(memoText)),
    });

    const intents: UserIntent[] = [
      {
        actor: payer.publicKey,
        ix: transferIx,
        priority: 0,
        accesses: [
          { pubkey: payer.publicKey, access: AccessKind.Writable },
          { pubkey: recipient.publicKey, access: AccessKind.Writable },
        ],
      },
      {
        actor: payer.publicKey,
        ix: memoIx,
        priority: 0,
        accesses: [],
      },
    ];

    // Get recent blockhash
    const { blockhash } = await connection.getLatestBlockhash('confirmed');

    // Build transaction
    const feePlan = await feePolicy.suggest();
    const tx = await buildTransaction({
      intents,
      feePlan,
      recentBlockhash: blockhash,
      payer: payer.publicKey,
    });

    // Submit
    const result = await submitter.submit(tx, [payer], {
      skipPreflight: false,
    });

    expect(result.signature).toBeDefined();

    // Confirm
    await connection.confirmTransaction(result.signature, 'confirmed');

    // Verify transaction details
    const txDetails = await connection.getTransaction(result.signature, {
      maxSupportedTransactionVersion: 0,
    });

    expect(txDetails).not.toBeNull();
    expect(txDetails?.meta?.err).toBeNull();
  }, 30000);
});
