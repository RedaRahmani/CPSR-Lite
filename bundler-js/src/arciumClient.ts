/**
 * Arcium client for queueing computations and awaiting results
 * Uses official SDK patterns and helpers
 */

import { Connection, Keypair, PublicKey, SystemProgram } from '@solana/web3.js';
import { AnchorProvider, Program, Idl, BN, EventParser } from '@coral-xyz/anchor';
import { awaitComputationFinalization } from '@arcium-hq/client';
import { PlannerInput, PlanOutput } from './types';
import {
  getMXEPublicKeyWithRetry,
  makeEphemeralKeypair,
  deriveSharedSecret,
  encryptPlannerInput,
  decryptPlanOutput,
  generateNonce,
  nonceToBytes,
  hashForLog,
} from './encrypt';

// Load IDL with flexible path. Preferred via env (ARCIUM_IDL_PATH).
// Fallback to vendored copy under bundler-js/src/idl/.
// This avoids tight monorepo coupling.
const confidentialPlannerIdl: any = (() => {
  const envPath = process.env.ARCIUM_IDL_PATH;
  if (envPath) {
    // Dynamic require to avoid build-time resolution
    return require(envPath);
  }
  try {
    // Vendored fallback (put a copy there if you don't build Anchor locally)
    // e.g. cp programs/confidential_planner/target/idl/confidential_planner.json bundler-js/src/idl/
    return require('./idl/confidential_planner.json');
  } catch {
    throw new Error(
      'IDL not found. Set ARCIUM_IDL_PATH to the built IDL JSON, or vendor it at bundler-js/src/idl/confidential_planner.json'
    );
  }
})();

// Arcium program ID (same across all deployments)
const ARCIUM_PROGRAM_ID = new PublicKey('Arc1umT7k7KwFY3yWg7Z3MhYpYQVBZN1bEPBYPKH4FpC');

/**
 * Queue a plan computation on-chain using proper Arcium SDK patterns
 */
export async function queuePlan(
  connection: Connection,
  programId: PublicKey,
  payer: Keypair,
  computationOffset: bigint,
  ciphertexts: number[][],
  clientPubkey: Uint8Array,
  nonce: bigint
): Promise<string> {
  // Use bundled IDL
  const idl: Idl = confidentialPlannerIdl as Idl;

  const provider = new AnchorProvider(
    connection,
    { publicKey: payer.publicKey } as any,
    { commitment: 'confirmed' }
  );

  // Anchor 0.30+: Program constructor is (idl, provider)
  // programId is derived from IDL metadata
  const program = new Program(idl, provider);

  // Convert offset to BN for Anchor
  const offsetBN = new BN(computationOffset.toString());

  // ciphertexts already number[][]
  const ciphertextArrays = ciphertexts;
  const clientPubkeyArray = Array.from(clientPubkey);
  const nonceBN = new BN(nonce.toString());

  // Call the instruction with proper argument order:
  // offset, ciphertexts (many scalars), client_pubkey, nonce
  const tx = await program.methods
    .planChunk(offsetBN, ciphertextArrays, clientPubkeyArray, nonceBN)
    .accounts({
      payer: payer.publicKey,
      // Other accounts are derived automatically by Anchor macros
      // from #[queue_computation_accounts] attribute
      systemProgram: SystemProgram.programId,
      arciumProgram: ARCIUM_PROGRAM_ID,
    })
    .signers([payer])
    .rpc();

  return tx;
}

/**
 * Parse PlanComputed event from transaction using Anchor's event parser
 */
export async function parseCallbackEvent(
  program: Program,
  txSig: string,
  connection: Connection
): Promise<{ offset: bigint; nonce: Uint8Array; ciphertexts: number[][] }> {
  const tx = await connection.getTransaction(txSig, {
    commitment: 'confirmed',
    maxSupportedTransactionVersion: 0,
  });

  if (!tx || !tx.meta || !tx.meta.logMessages) {
    throw new Error('Transaction not found or has no logs');
  }

  // Use Anchor's event parser: pass the WHOLE log array once
  const eventParser = new EventParser(program.programId, program.coder);
  const events: any[] = [];
  for (const e of eventParser.parseLogs(tx.meta.logMessages)) {
    events.push(e);
  }

  // Find either full or hash-only event
  const planComputedEvent =
    events.find(e => e.name === 'planComputed' || e.name === 'PlanComputed') ||
    events.find(e => e.name === 'planComputedHash' || e.name === 'PlanComputedHash');
  if (!planComputedEvent) {
    throw new Error('PlanComputed/PlanComputedHash event not found in transaction logs');
  }

  const data = planComputedEvent.data;
  
  // Extract fields from event
  const offset = BigInt(data.offset.toString());
  const nonce = new Uint8Array(data.nonce);
  // If this is the hash-only event, ciphertexts won't exist
  const ciphertexts = Array.isArray(data.ciphertexts)
    ? data.ciphertexts as number[][]
    : [];

  return { offset, nonce, ciphertexts };
}

/**
 * High-level: encrypt, queue, await, decrypt
 * Uses official Arcium SDK patterns throughout
 */
export async function runOnce(
  rpcUrl: string,
  programId: PublicKey,
  payer: Keypair,
  dag: PlannerInput,
  timeoutMs: number
): Promise<{
  plan: PlanOutput;
  offset: bigint;
  finalizeTx: string;
  metrics: {
    encryptMs: number;
    queueMs: number;
    awaitMs: number;
    decryptMs: number;
    totalMs: number;
  };
}> {
  const totalStart = Date.now();
  
  // Early return for empty input
  if (!dag.intents || dag.intents.length === 0 || dag.count === 0) {
    console.error(`[bundler-js] Empty input, returning empty plan`);
    return {
      plan: { chunkCount: 0, chunks: [] },
      offset: 0n,
      finalizeTx: '',
      metrics: {
        encryptMs: 0,
        queueMs: 0,
        awaitMs: 0,
        decryptMs: 0,
        totalMs: Date.now() - totalStart,
      },
    };
  }
  
  const connection = new Connection(rpcUrl, 'confirmed');

  // Use bundled IDL
  const idl: Idl = confidentialPlannerIdl as Idl;
  
  const provider = new AnchorProvider(
    connection,
    { publicKey: payer.publicKey } as any,
    { commitment: 'confirmed' }
  );
  // Anchor 0.30+: Program constructor is (idl, provider)
  const program = new Program(idl, provider);

  // Step 1: Fetch MXE public key using SDK helper
  const encryptStart = Date.now();
  const mxePubkey = await getMXEPublicKeyWithRetry(connection, programId);

  // Step 2: Generate ephemeral keypair
  const ephemeral = makeEphemeralKeypair();

  // Step 3: Derive shared secret using SDK x25519
  const sharedSecret = deriveSharedSecret(ephemeral.secretKey, mxePubkey);

  // Step 4: Generate nonce
  const nonce = generateNonce();
  const nonceBytes = nonceToBytes(nonce);

  // Debug logging (hash sensitive data for security)
  console.error(`[bundler-js] Encryption setup: sharedSecret=${hashForLog(sharedSecret)}, nonce=${hashForLog(nonceBytes)}, nonce_endianness=LE`);

  // Step 5: Encrypt input using official RescueCipher
  const ciphertexts = encryptPlannerInput(dag, sharedSecret, nonceBytes);
  console.error(`[bundler-js] Encrypted input: ciphertexts_len=${ciphertexts.length}`);
  const encryptMs = Date.now() - encryptStart;

  // Step 6: Queue computation with atomic offset (timestamp + random for uniqueness)
  const queueStart = Date.now();
  const random = Math.floor(Math.random() * 0xFFFF); // 16-bit random
  const computationOffset = (BigInt(Date.now()) << 16n) | BigInt(random);
  console.error(`[bundler-js] Computation offset: ${computationOffset.toString()}`);
  
  const queueTxSig = await queuePlan(
    connection,
    programId,
    payer,
    computationOffset,
    ciphertexts,
    ephemeral.publicKey,
    nonce
  );
  console.error(`[bundler-js] Queued computation: ${queueTxSig}`);
  const queueMs = Date.now() - queueStart;

  // Step 7: Await finalization using official SDK helper with retries
  const awaitStart = Date.now();
  let finalizeTxSig: string | undefined;
  let lastError: Error | undefined;
  
  console.error(`[bundler-js] ⏳ Waiting for MPC workers to process computation...`);
  console.error(`[bundler-js] 💡 This requires active ARX nodes for cluster offset`);
  console.error(`[bundler-js] 🔍 If this hangs, check:`);
  console.error(`[bundler-js]    - Run: arcium arx-active`);
  console.error(`[bundler-js]    - Setup: bash bundler-js/scripts/setup_arx_node.sh`);
  console.error(`[bundler-js]    - Logs: tail -f ~/.arcium/logs/arx.log`);
  
  const maxRetries = 3;
  for (let attempt = 1; attempt <= maxRetries; attempt++) {
    try {
      console.error(`[bundler-js] Awaiting computation finalization (attempt ${attempt}/${maxRetries}, timeout: ${timeoutMs}ms)...`);
      finalizeTxSig = await awaitComputationFinalization(
        provider,
        computationOffset,
        programId,
        'confirmed'
      );
      console.error(`[bundler-js] ✅ Computation finalized: ${finalizeTxSig}`);
      break; // Success, exit retry loop
    } catch (err: any) {
      lastError = err;
      console.error(`[bundler-js] ⚠️  Finalization attempt ${attempt} failed: ${err.message}`);
      
      if (err.message?.includes('timeout') || err.message?.includes('timed out')) {
        console.error(`[bundler-js] ❌ TIMEOUT: No MPC workers responded`);
        console.error(`[bundler-js] 🚨 This means there are NO active ARX nodes processing computations`);
        console.error(`[bundler-js] 🔧 To fix: Run 'bash bundler-js/scripts/setup_arx_node.sh' to start a worker`);
      }
      
      if (attempt < maxRetries) {
        const delayMs = 1000; // 1 second delay between retries
        console.error(`[bundler-js] Retrying in ${delayMs}ms...`);
        await new Promise(resolve => setTimeout(resolve, delayMs));
      }
    }
  }
  
  if (!finalizeTxSig) {
    throw new Error(`Computation finalization failed after ${maxRetries} retries: ${lastError?.message || 'unknown error'}`);
  }
  
  const awaitMs = Date.now() - awaitStart;

  // Step 8: Parse callback event using Anchor's event parser
  const eventData = await parseCallbackEvent(program, finalizeTxSig, connection);
  
  // Debug logging for event data
  console.error(`[bundler-js] Event data: offset=${eventData.offset.toString()}, nonce=${hashForLog(eventData.nonce)}, ciphertexts_len=${eventData.ciphertexts.length}`);
  
  // Validate nonce matches
  const eventNonceBytes = eventData.nonce;
  let nonceMatches = true;
  if (eventNonceBytes.length !== nonceBytes.length) {
    nonceMatches = false;
  } else {
    for (let i = 0; i < nonceBytes.length; i++) {
      if (eventNonceBytes[i] !== nonceBytes[i]) {
        nonceMatches = false;
        break;
      }
    }
  }
  
  if (!nonceMatches) {
    console.error(`[bundler-js] Nonce validation: FAIL - Expected ${hashForLog(nonceBytes)}, got ${hashForLog(eventNonceBytes)}`);
    throw new Error('Nonce mismatch between request and callback event');
  }
  console.error(`[bundler-js] Nonce validation: PASS`);

  // Step 9: Decrypt or fail fast depending on event type
  if (!eventData.ciphertexts.length) {
    console.error('[bundler-js] No ciphertexts in event (PlanComputedHash). ' +
      'You built the program without full-event-logs. Either enable the feature during dev ' +
      'or implement on-chain storage + client fetch for ciphertexts.');
    throw new Error('No ciphertexts available from event; cannot decrypt output');
  }
  const decryptStart = Date.now();
  const plan = decryptPlanOutput(eventData.ciphertexts, sharedSecret, eventData.nonce);
  console.error(`[bundler-js] Decryption: unpack_len_expected=129, unpack_len_actual=${eventData.ciphertexts.length}, chunk_count=${plan.chunkCount}`);
  const decryptMs = Date.now() - decryptStart;

  const totalMs = Date.now() - totalStart;

  return {
    plan,
    offset: computationOffset,
    finalizeTx: finalizeTxSig,
    metrics: {
      encryptMs,
      queueMs,
      awaitMs,
      decryptMs,
      totalMs,
    },
  };
}
