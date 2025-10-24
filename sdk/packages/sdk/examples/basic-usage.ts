/**
 * Example usage of the CPSR SDK
 * 
 * This example demonstrates:
 * - Creating a client
 * - Building intents
 * - Submitting transactions via L1 and Router
 * - Using different fee policies
 */

import { Connection, Keypair, LAMPORTS_PER_SOL } from '@solana/web3.js';
import {
  CpsrClient,
  transferIntent,
  memoIntent,
  customIntent,
  SubmitLane,
  BasicPolicy,
  StaticAltSource,
} from '@cpsr/sdk';

async function basicExample() {
  // 1. Setup connection and payer
  const connection = new Connection('https://api.devnet.solana.com', 'confirmed');
  const payer = Keypair.generate(); // In production, load from file/env

  // Airdrop some SOL for testing (devnet only)
  console.log('Requesting airdrop...');
  const airdropSignature = await connection.requestAirdrop(
    payer.publicKey,
    2 * LAMPORTS_PER_SOL
  );
  await connection.confirmTransaction(airdropSignature);
  console.log('Airdrop confirmed');

  // 2. Create CPSR client
  const client = new CpsrClient({
    connection,
    payer,
    feePolicy: new BasicPolicy(1_400_000, 100, 5_000),
  });

  // 3. Build a transfer intent
  const recipient = Keypair.generate().publicKey;
  const intent = transferIntent({
    from: payer.publicKey,
    to: recipient,
    lamports: 100_000,
  });

  // 4. Submit via L1
  console.log('Submitting transaction via L1...');
  const result = await client.buildAndSubmit({
    intents: [intent],
    cuPrice: 1_000,
    cuLimit: 200_000,
    submitOptions: {
      lane: SubmitLane.L1,
      skipPreflight: false,
    },
  });

  console.log('✓ Transaction confirmed!');
  console.log('  Signature:', result.signature);
  console.log('  Lane:', result.lane);
  console.log('  Attempts:', result.attempts);

  // Verify balance
  const balance = await connection.getBalance(recipient);
  console.log('  Recipient balance:', balance, 'lamports');
}

async function recentFeesExample() {
  const connection = new Connection('https://api.devnet.solana.com');
  const payer = Keypair.generate();

  // Create client with adaptive fee policy
  const client = CpsrClient.withRecentFees({
    connection,
    payer,
    percentile: 0.75, // Use p75 of recent fees
    useEma: true, // Smooth fees with EMA
    useHysteresis: true, // Avoid frequent changes
  });

  const intent = transferIntent({
    from: payer.publicKey,
    to: Keypair.generate().publicKey,
    lamports: 50_000,
  });

  // The client will automatically fetch recent fees and set appropriate price
  const result = await client.buildAndSubmit({
    intents: [intent],
    submitOptions: { lane: SubmitLane.L1 },
  });

  console.log('Transaction with adaptive fees:', result.signature);
}

async function routerExample() {
  const connection = new Connection('https://api.devnet.solana.com');
  const payer = Keypair.generate();

  // Create client with Router support
  const client = new CpsrClient({
    connection,
    payer,
    routerUrl: 'https://devnet-router.magicblock.app',
    routerApiKey: process.env.MAGIC_ROUTER_API_KEY, // Optional
  });

  const intent = transferIntent({
    from: payer.publicKey,
    to: Keypair.generate().publicKey,
    lamports: 75_000,
  });

  // Submit via Magic Router
  const result = await client.buildAndSubmit({
    intents: [intent],
    cuPrice: 5_000,
    cuLimit: 200_000,
    submitOptions: {
      lane: SubmitLane.Router,
      skipPreflight: true, // Router handles preflight
    },
  });

  console.log('Transaction via Router:', result.signature);
}

async function multipleIntentsExample() {
  const connection = new Connection('https://api.devnet.solana.com');
  const payer = Keypair.generate();
  const client = new CpsrClient({ connection, payer });

  // Build multiple intents
  const recipient1 = Keypair.generate().publicKey;
  const recipient2 = Keypair.generate().publicKey;

  const intents = [
    transferIntent({
      from: payer.publicKey,
      to: recipient1,
      lamports: 100_000,
      options: { priority: 10 }, // Higher priority
    }),
    transferIntent({
      from: payer.publicKey,
      to: recipient2,
      lamports: 50_000,
      options: { priority: 5 },
    }),
    memoIntent({
      payer: payer.publicKey,
      message: 'Batch transfer via CPSR SDK',
    }),
  ];

  const result = await client.buildAndSubmit({
    intents,
    cuPrice: 2_000,
    cuLimit: 300_000,
  });

  console.log('Batch transaction:', result.signature);
}

async function tightenCUExample() {
  const connection = new Connection('https://api.devnet.solana.com');
  const payer = Keypair.generate();
  const client = new CpsrClient({ connection, payer });

  const intent = transferIntent({
    from: payer.publicKey,
    to: Keypair.generate().publicKey,
    lamports: 100_000,
  });

  // Build with CU tightening (simulates first to determine actual usage)
  const tx = await client.build({
    intents: [intent],
    cuLimit: 1_000_000, // Start with high limit
    cuPrice: 1_000,
    tighten: true, // SDK will simulate and adjust
  });

  console.log('Transaction built with tightened CU limit');

  const result = await client.submit(tx, { lane: SubmitLane.L1 });
  console.log('Optimized transaction:', result.signature);
}

async function errorHandlingExample() {
  const connection = new Connection('https://api.devnet.solana.com');
  const payer = Keypair.generate();
  const client = new CpsrClient({ connection, payer });

  try {
    const intent = transferIntent({
      from: payer.publicKey,
      to: Keypair.generate().publicKey,
      lamports: 1_000_000_000, // Way more than we have
    });

    await client.buildAndSubmit({
      intents: [intent],
      cuPrice: 1_000,
      cuLimit: 200_000,
    });
  } catch (error) {
    if (error instanceof Error) {
      console.error('Transaction failed:', error.message);
      
      // Check error type for specific handling
      if (error.name === 'PreflightFailedError') {
        console.log('Preflight simulation failed - transaction would fail on-chain');
      } else if (error.name === 'BlockhashStaleError') {
        console.log('Blockhash expired - retry with fresh blockhash');
      } else if (error.name === 'RpcRetriableError') {
        console.log('RPC error - retry with backoff');
      }
    }
  }
}

// Run examples
if (require.main === module) {
  console.log('CPSR SDK Examples\n');
  
  basicExample()
    .then(() => console.log('\n✓ Basic example complete'))
    .catch(console.error);

  // Uncomment to run other examples:
  // recentFeesExample().catch(console.error);
  // routerExample().catch(console.error);
  // multipleIntentsExample().catch(console.error);
  // tightenCUExample().catch(console.error);
  // errorHandlingExample().catch(console.error);
}

export {
  basicExample,
  recentFeesExample,
  routerExample,
  multipleIntentsExample,
  tightenCUExample,
  errorHandlingExample,
};
