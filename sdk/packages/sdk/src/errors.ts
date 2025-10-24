/**
 * Error taxonomy for CPSR SDK operations
 * @module errors
 */

/**
 * Base error class for all CPSR SDK errors
 */
export class CpsrError extends Error {
  constructor(
    message: string,
    public readonly code: string,
    public readonly retriable: boolean = false
  ) {
    super(message);
    this.name = 'CpsrError';
    Object.setPrototypeOf(this, CpsrError.prototype);
  }
}

/**
 * Thrown when a blockhash becomes stale before submission
 */
export class BlockhashStaleError extends CpsrError {
  constructor(message: string = 'Blockhash is stale') {
    super(message, 'ERR_BLOCKHASH_STALE', true);
    this.name = 'BlockhashStaleError';
    Object.setPrototypeOf(this, BlockhashStaleError.prototype);
  }
}

/**
 * Thrown when preflight simulation fails
 */
export class PreflightFailedError extends CpsrError {
  constructor(
    message: string,
    public readonly logs?: string[]
  ) {
    super(message, 'ERR_PREFLIGHT_FAIL', false);
    this.name = 'PreflightFailedError';
    Object.setPrototypeOf(this, PreflightFailedError.prototype);
  }
}

/**
 * Thrown when RPC returns rate-limit (429) or similar retriable errors
 */
export class RpcRetriableError extends CpsrError {
  constructor(
    message: string,
    public readonly statusCode?: number
  ) {
    super(message, 'ERR_RPC_429', true);
    this.name = 'RpcRetriableError';
    Object.setPrototypeOf(this, RpcRetriableError.prototype);
  }
}

/**
 * Thrown when transaction exceeds size limits
 */
export class TransactionTooLargeError extends CpsrError {
  constructor(
    message: string,
    public readonly actualSize: number,
    public readonly maxSize: number
  ) {
    super(message, 'ERR_TX_TOO_LARGE', false);
    this.name = 'TransactionTooLargeError';
    Object.setPrototypeOf(this, TransactionTooLargeError.prototype);
  }
}

/**
 * Thrown when Router returns a JSON-RPC error
 */
export class RouterJsonRpcError extends CpsrError {
  constructor(
    public readonly rpcCode: number,
    message: string
  ) {
    super(message, 'ERR_ROUTER_JSONRPC', false);
    this.name = 'RouterJsonRpcError';
    Object.setPrototypeOf(this, RouterJsonRpcError.prototype);
  }
}

/**
 * Thrown when a network or timeout error occurs
 */
export class NetworkError extends CpsrError {
  constructor(message: string, cause?: Error) {
    super(message, 'ERR_NETWORK', true);
    this.name = 'NetworkError';
    this.cause = cause;
    Object.setPrototypeOf(this, NetworkError.prototype);
  }
}

/**
 * Thrown when intent validation fails
 */
export class InvalidIntentError extends CpsrError {
  constructor(message: string) {
    super(message, 'ERR_INVALID_INTENT', false);
    this.name = 'InvalidIntentError';
    Object.setPrototypeOf(this, InvalidIntentError.prototype);
  }
}
