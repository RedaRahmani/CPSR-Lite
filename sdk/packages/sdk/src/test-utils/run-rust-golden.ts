/**
 * Helper to run Rust golden serializer binary
 */

import { exec } from 'child_process';
import { promisify } from 'util';
import { readFile } from 'fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

// Resolve to repo root from sdk/packages/sdk/src/test-utils/
const REPO_ROOT = resolve(__dirname, '../../../../..');
const BUNDLER_CARGO_TOML = join(REPO_ROOT, 'bundler/Cargo.toml');

const execAsync = promisify(exec);

export interface RustGoldenOutput {
  description: string;
  message_base64: string;
  message_bytes: number;
}

/**
 * Run the Rust golden helper on a golden JSON file
 * @param goldenPath - Path to the golden JSON file
 * @returns Parsed output from Rust helper
 */
export async function runRustGolden(goldenPath: string): Promise<RustGoldenOutput> {
  try {
    const { stdout, stderr } = await execAsync(
      `cat "${goldenPath}" | cargo run --quiet --manifest-path "${BUNDLER_CARGO_TOML}" --example serializer_golden`,
      {
        cwd: REPO_ROOT,
        maxBuffer: 10 * 1024 * 1024, // 10MB buffer
      }
    );

    if (stderr && !stderr.includes('Finished') && !stderr.includes('Running')) {
      console.warn('Rust golden stderr:', stderr);
    }

    const output = JSON.parse(stdout.trim()) as RustGoldenOutput;
    return output;
  } catch (error) {
    if (error instanceof Error) {
      throw new Error(`Failed to run Rust golden helper: ${error.message}`);
    }
    throw error;
  }
}

/**
 * Read a golden JSON file
 * @param goldenPath - Path to the golden JSON file
 * @returns Parsed golden test case
 */
export async function readGolden(goldenPath: string): Promise<GoldenTestCase> {
  const content = await readFile(goldenPath, 'utf-8');
  return JSON.parse(content) as GoldenTestCase;
}

export interface GoldenTestCase {
  description: string;
  payer: string;
  recentBlockhash: string;
  cuLimit: number;
  cuPriceMicrolamports: number;
  memo?: string;
  transfers: Transfer[];
  alts: {
    lookups: AltLookup[];
  };
}

export interface Transfer {
  from: string;
  to: string;
  lamports: number;
}

export interface AltLookup {
  tablePubkey: string;
  writableIndexes: number[];
  readonlyIndexes: number[];
}
