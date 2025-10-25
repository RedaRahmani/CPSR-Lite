#!/usr/bin/env node
/**
 * CLI entry point for Arcium encrypted planner
 * Reads JSON from stdin, writes JSON to stdout
 * All diagnostics go to stderr
 */

import { Keypair } from '@solana/web3.js';
import { PublicKey } from '@solana/web3.js';
import { CliInput, CliOutput } from './types';
import { runOnce } from './arciumClient';
import * as fs from 'fs';

/**
 * Read complete stdin as string
 */
async function readStdin(): Promise<string> {
  return new Promise((resolve, reject) => {
    let data = '';
    process.stdin.setEncoding('utf8');
    
    process.stdin.on('data', (chunk) => {
      data += chunk;
    });
    
    process.stdin.on('end', () => {
      resolve(data);
    });
    
    process.stdin.on('error', (err) => {
      reject(err);
    });
  });
}

/**
 * Load keypair from filesystem (default Solana location)
 */
function loadKeypair(): Keypair {
  const keypairPath = process.env.SOLANA_KEYPAIR_PATH || 
    `${process.env.HOME}/.config/solana/id.json`;
  
  try {
    const secretKeyString = fs.readFileSync(keypairPath, 'utf8');
    const secretKey = Uint8Array.from(JSON.parse(secretKeyString));
    return Keypair.fromSecretKey(secretKey);
  } catch (err) {
    throw new Error(`Failed to load keypair from ${keypairPath}: ${err}`);
  }
}

/**
 * Main CLI logic
 */
async function main() {
  try {
    // Read JSON from stdin
    const inputJson = await readStdin();
    console.error(`[bundler-js] Received JSON (${inputJson.length} bytes):\n${inputJson.substring(0, 500)}`);
    
    const input: CliInput = JSON.parse(inputJson);
    console.error(`[bundler-js] Parsed input:`, JSON.stringify({
      rpcUrl: input.rpcUrl,
      programId: input.programId,
      cluster: input.cluster,
      dagIntentsCount: input.dag?.intents?.length,
      dagCount: input.dag?.count,
      timeoutMs: input.timeoutMs,
    }));
    
    // Validate input
    if (!input.rpcUrl || !input.programId || !input.dag) {
      throw new Error('Missing required fields: rpcUrl, programId, dag');
    }
    
    const timeoutMs = input.timeoutMs || 5000;
    
    // Load payer keypair
    const payer = loadKeypair();
    
    // Log to stderr for diagnostics
    console.error(`[bundler-js] Starting encrypted planner`);
    console.error(`[bundler-js] RPC: ${input.rpcUrl}`);
    console.error(`[bundler-js] Program: ${input.programId}`);
    console.error(`[bundler-js] Intent count: ${input.dag.count}`);
    console.error(`[bundler-js] Timeout: ${timeoutMs}ms`);
    console.error(`[bundler-js] Input JSON length: ${inputJson.length} bytes`);
    
    // Run the encrypted planner flow
    const programId = new PublicKey(input.programId);
    const result = await runOnce(
      input.rpcUrl,
      programId,
      payer,
      input.dag,
      timeoutMs
    );
    
    // Success output to stdout
    const output: CliOutput = {
      ok: true,
      plan: result.plan,
      offset: result.offset.toString(),
      finalizeTx: result.finalizeTx,
      metrics: result.metrics,
    };
    
    console.error(`[bundler-js] Success! Chunks: ${result.plan.chunkCount}, Latency: ${result.metrics.totalMs}ms, Output scalars: 129 (Option A)`);
    console.log(JSON.stringify(output));
    
  } catch (err: any) {
    // Error output to stdout (still JSON)
    const output: CliOutput = {
      ok: false,
      error: err.message || String(err),
    };
    
    console.error(`[bundler-js] Error: ${err.message || err}`);
    console.log(JSON.stringify(output));
    
    process.exit(1);
  }
}

// Run main
main().catch((err) => {
  console.error(`[bundler-js] Fatal error: ${err}`);
  process.exit(1);
});
