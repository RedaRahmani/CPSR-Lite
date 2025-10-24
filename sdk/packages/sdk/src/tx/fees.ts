/**
 * Fee policies for compute budget management
 * @module tx/fees
 */

import { Connection } from '@solana/web3.js';
import { RpcRetriableError } from '../errors.js';

/**
 * Fee plan specifying compute units and price
 */
export interface FeePlan {
  /** Compute unit limit */
  cuLimit: number;
  /** Price in micro-lamports per CU */
  cuPriceMicroLamports: number;
}

/**
 * Fee policy interface
 */
export interface FeePolicy {
  /**
   * Suggest an initial fee plan
   */
  suggest(): Promise<FeePlan>;
  
  /**
   * Clamp a fee plan to policy limits
   */
  clamp(plan: FeePlan): FeePlan;
}

/**
 * Basic fee policy with static floors and ceilings
 */
export class BasicPolicy implements FeePolicy {
  constructor(
    private readonly maxCuLimit: number = 1_400_000,
    private readonly minCuPrice: number = 100,
    private readonly maxCuPrice: number = 5_000
  ) {}

  async suggest(): Promise<FeePlan> {
    return {
      cuLimit: this.maxCuLimit,
      cuPriceMicroLamports: this.minCuPrice,
    };
  }

  clamp(plan: FeePlan): FeePlan {
    return {
      cuLimit: Math.min(plan.cuLimit, this.maxCuLimit),
      cuPriceMicroLamports: Math.max(
        this.minCuPrice,
        Math.min(plan.cuPriceMicroLamports, this.maxCuPrice)
      ),
    };
  }
}

/**
 * Options for RecentFeesPolicy
 */
export interface RecentFeesPolicyOptions {
  /** Maximum CU limit */
  maxCuLimit?: number;
  /** Minimum CU price (micro-lamports) */
  minCuPrice?: number;
  /** Maximum CU price (micro-lamports) */
  maxCuPrice?: number;
  /** Percentile to use (0-1, default 0.75 for p75) */
  percentile?: number;
  /** Enable EMA smoothing */
  useEma?: boolean;
  /** EMA alpha (0-1, default 0.3) */
  emaAlpha?: number;
  /** Enable hysteresis (avoid frequent changes) */
  useHysteresis?: boolean;
  /** Hysteresis threshold (fraction, default 0.1 = 10%) */
  hysteresisThreshold?: number;
  /** Number of recent samples to fetch */
  sampleSize?: number;
}

/**
 * Fee policy based on recent prioritization fees from RPC
 * 
 * Features:
 * - Samples getRecentPrioritizationFees
 * - Computes percentile (default p75)
 * - Optional EMA smoothing to reduce volatility
 * - Optional hysteresis to avoid frequent price changes
 */
export class RecentFeesPolicy implements FeePolicy {
  private readonly maxCuLimit: number;
  private readonly minCuPrice: number;
  private readonly maxCuPrice: number;
  private readonly percentile: number;
  private readonly useEma: boolean;
  private readonly emaAlpha: number;
  private readonly useHysteresis: boolean;
  private readonly hysteresisThreshold: number;
  private readonly sampleSize: number;

  private emaState: number | null = null;
  private lastPrice: number | null = null;

  constructor(
    private readonly connection: Connection,
    options: RecentFeesPolicyOptions = {}
  ) {
    this.maxCuLimit = options.maxCuLimit ?? 1_400_000;
    this.minCuPrice = options.minCuPrice ?? 100;
    this.maxCuPrice = options.maxCuPrice ?? 10_000;
    this.percentile = options.percentile ?? 0.75;
    this.useEma = options.useEma ?? false;
    this.emaAlpha = options.emaAlpha ?? 0.3;
    this.useHysteresis = options.useHysteresis ?? false;
    this.hysteresisThreshold = options.hysteresisThreshold ?? 0.1;
    this.sampleSize = options.sampleSize ?? 150;
  }

  async suggest(): Promise<FeePlan> {
    try {
      const fees = await this.connection.getRecentPrioritizationFees({
        lockedWritableAccounts: [],
      });

      if (fees.length === 0) {
        return this.fallbackPlan();
      }

      // Extract prioritization fees (in micro-lamports per CU)
      const prices = fees
        .map((f) => f.prioritizationFee)
        .filter((p) => p > 0)
        .slice(0, this.sampleSize);

      if (prices.length === 0) {
        return this.fallbackPlan();
      }

      prices.sort((a, b) => a - b);
      const idx = Math.min(
        Math.floor(prices.length * this.percentile),
        prices.length - 1
      );
      let rawPrice = prices[idx];

      // Apply EMA smoothing if enabled
      if (this.useEma) {
        if (this.emaState === null) {
          this.emaState = rawPrice;
        } else {
          this.emaState =
            this.emaAlpha * rawPrice + (1 - this.emaAlpha) * this.emaState;
        }
        rawPrice = Math.round(this.emaState);
      }

      // Apply hysteresis if enabled
      if (this.useHysteresis && this.lastPrice !== null) {
        const delta = Math.abs(rawPrice - this.lastPrice);
        const threshold = this.lastPrice * this.hysteresisThreshold;
        if (delta < threshold) {
          // Change too small, keep old price
          rawPrice = this.lastPrice;
        }
      }

      this.lastPrice = rawPrice;

      return this.clamp({
        cuLimit: this.maxCuLimit,
        cuPriceMicroLamports: rawPrice,
      });
    } catch (error) {
      // If RPC fails (rate limit, timeout), return fallback
      if (error instanceof Error && error.message.includes('429')) {
        throw new RpcRetriableError('Rate limited fetching recent fees', 429);
      }
      return this.fallbackPlan();
    }
  }

  clamp(plan: FeePlan): FeePlan {
    return {
      cuLimit: Math.min(plan.cuLimit, this.maxCuLimit),
      cuPriceMicroLamports: Math.max(
        this.minCuPrice,
        Math.min(plan.cuPriceMicroLamports, this.maxCuPrice)
      ),
    };
  }

  private fallbackPlan(): FeePlan {
    return {
      cuLimit: this.maxCuLimit,
      cuPriceMicroLamports: this.minCuPrice,
    };
  }

  /**
   * Reset internal state (EMA, hysteresis)
   */
  reset(): void {
    this.emaState = null;
    this.lastPrice = null;
  }
}
