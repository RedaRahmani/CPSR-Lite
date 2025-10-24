/**
 * Submission types and interfaces
 * @module submit/types
 */

/**
 * Submit lane selection
 */
export enum SubmitLane {
  /** Direct L1 submission via RPC */
  L1 = 'l1',
  /** Magic Router submission */
  Router = 'router',
}

/**
 * Options for transaction submission
 */
export interface SubmitOptions {
  /** Submit lane (default: L1) */
  lane?: SubmitLane;
  /** Skip preflight simulation (default: false) */
  skipPreflight?: boolean;
  /** Max retries for retriable errors (default: 3) */
  maxRetries?: number;
}

/**
 * Result of transaction submission
 */
export interface SubmitResult {
  /** Transaction signature */
  signature: string;
  /** Lane used for submission */
  lane: SubmitLane;
  /** Whether preflight was skipped */
  preflightSkipped: boolean;
  /** Number of attempts made */
  attempts: number;
}
