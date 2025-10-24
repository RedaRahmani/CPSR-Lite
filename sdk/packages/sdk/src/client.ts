/**
 * Main CPSR SDK client
 * @module client
 */

import { Connection, Keypair, PublicKey, Signer } from '@solana/web3.js';
import { BasicPolicy, FeePolicy, RecentFeesPolicy } from './tx/fees.js';
import { buildTransaction, buildAndTighten, BuildTransactionOptions } from './tx/builder.js';
import { AddressLookupSource, NoAltSource } from './tx/alt.js';
import { L1Submitter } from './submit/l1.js';
import { RouterSubmitter } from './submit/router.js';
import { SubmitLane, SubmitOptions, SubmitResult } from './submit/types.js';
import { Logger, LogLevel } from './utils/logger.js';

/**
 * CPSR Client configuration
 */
export interface CpsrClientConfig {
  /** Solana RPC connection */
  connection: Connection;
  /** Default payer for transactions */
  payer: Keypair;
  /** Router URL (for Router lane) */
  routerUrl?: string;
  /** Router API key (optional) */
  routerApiKey?: string;
  /** Fee policy (default: BasicPolicy) */
  feePolicy?: FeePolicy;
  /** ALT source (default: NoAltSource) */
  altSource?: AddressLookupSource;
  /** Logger level */
  logLevel?: LogLevel;
}

/**
 * Build options for the client
 */
export interface ClientBuildOptions {
  /** Compute unit price (micro-lamports per CU) */
  cuPrice?: number;
  /** Compute unit limit */
  cuLimit?: number;
  /** Enable automatic CU tightening via simulation */
  tighten?: boolean;
}

/**
 * Main CPSR Client
 * 
 * Provides a unified interface for:
 * - Building transactions with compute budgets
 * - Resolving ALTs
 * - Submitting via L1 or Router lanes
 */
export class CpsrClient {
  private readonly connection: Connection;
  private readonly payer: Keypair;
  private readonly feePolicy: FeePolicy;
  private readonly altSource: AddressLookupSource;
  private readonly l1Submitter: L1Submitter;
  private readonly routerSubmitter: RouterSubmitter | null;
  private readonly logger: Logger;

  constructor(config: CpsrClientConfig) {
    this.connection = config.connection;
    this.payer = config.payer;
    this.feePolicy = config.feePolicy ?? new BasicPolicy();
    this.altSource = config.altSource ?? new NoAltSource();
    this.l1Submitter = new L1Submitter(this.connection);
    
    if (config.routerUrl) {
      this.routerSubmitter = new RouterSubmitter(
        config.routerUrl,
        config.routerApiKey
      );
    } else {
      this.routerSubmitter = null;
    }

    this.logger = new Logger(config.logLevel ?? LogLevel.INFO, 'cpsr-client');
  }

  /**
   * Build a transaction from build options
   */
  async build(options: ClientBuildOptions & { intents: BuildTransactionOptions['intents'] }) {
    const { intents, cuPrice, cuLimit, tighten = false } = options;

    // Get fee plan
    const suggestedPlan = await this.feePolicy.suggest();
    const feePlan = this.feePolicy.clamp({
      cuLimit: cuLimit ?? suggestedPlan.cuLimit,
      cuPriceMicroLamports: cuPrice ?? suggestedPlan.cuPriceMicroLamports,
    });

    this.logger.info(
      `Building transaction with ${intents.length} intents, CU limit=${feePlan.cuLimit}, price=${feePlan.cuPriceMicroLamports}`
    );

    // Get recent blockhash
    const { blockhash } = await this.connection.getLatestBlockhash('finalized');

    const buildOpts: BuildTransactionOptions = {
      intents,
      feePlan,
      recentBlockhash: blockhash,
      payer: this.payer.publicKey,
      altSource: this.altSource,
    };

    if (tighten) {
      const result = await buildAndTighten(this.connection, buildOpts);
      this.logger.info(
        `Tightened CU limit from ${feePlan.cuLimit} to ${result.finalPlan.cuLimit} (used: ${result.usedCU})`
      );
      return result.tx;
    }

    return buildTransaction(buildOpts);
  }

  /**
   * Submit a transaction
   */
  async submit(
    transaction: ReturnType<typeof buildTransaction> extends Promise<infer T> ? T : never,
    options: SubmitOptions = {}
  ): Promise<SubmitResult> {
    const lane = options.lane ?? SubmitLane.L1;
    const signers: Signer[] = [this.payer];

    this.logger.info(`Submitting via ${lane} lane`);

    if (lane === SubmitLane.Router) {
      if (!this.routerSubmitter) {
        throw new Error('Router URL not configured');
      }
      return this.routerSubmitter.submit(transaction, signers, options);
    }

    return this.l1Submitter.submit(transaction, signers, options);
  }

  /**
   * Build and submit in one call
   */
  async buildAndSubmit(
    options: ClientBuildOptions & {
      intents: BuildTransactionOptions['intents'];
      submitOptions?: SubmitOptions;
    }
  ): Promise<SubmitResult> {
    const { submitOptions, ...buildOptions } = options;
    const tx = await this.build(buildOptions);
    return this.submit(tx, submitOptions);
  }

  /**
   * Get the connection
   */
  getConnection(): Connection {
    return this.connection;
  }

  /**
   * Get the payer public key
   */
  getPayer(): PublicKey {
    return this.payer.publicKey;
  }

  /**
   * Create a new client with RecentFeesPolicy
   */
  static withRecentFees(
    config: Omit<CpsrClientConfig, 'feePolicy'> & {
      percentile?: number;
      useEma?: boolean;
      useHysteresis?: boolean;
    }
  ): CpsrClient {
    const { percentile, useEma, useHysteresis, ...baseConfig } = config;
    return new CpsrClient({
      ...baseConfig,
      feePolicy: new RecentFeesPolicy(baseConfig.connection, {
        percentile,
        useEma,
        useHysteresis,
      }),
    });
  }
}
