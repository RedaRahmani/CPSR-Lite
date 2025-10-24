/**
 * Unit tests for fee policies
 */

import { Connection } from '@solana/web3.js';
import { BasicPolicy, RecentFeesPolicy } from '../../src/tx/fees';

describe('BasicPolicy', () => {
  it('should suggest default limits', async () => {
    const policy = new BasicPolicy();
    const plan = await policy.suggest();
    
    expect(plan.cuLimit).toBe(1_400_000);
    expect(plan.cuPriceMicroLamports).toBe(100);
  });

  it('should clamp to policy limits', () => {
    const policy = new BasicPolicy(1_400_000, 100, 5_000);
    
    const plan = policy.clamp({
      cuLimit: 2_000_000,
      cuPriceMicroLamports: 10_000,
    });
    
    expect(plan.cuLimit).toBe(1_400_000);
    expect(plan.cuPriceMicroLamports).toBe(5_000);
  });

  it('should enforce minimum price', () => {
    const policy = new BasicPolicy(1_400_000, 100, 5_000);
    
    const plan = policy.clamp({
      cuLimit: 100_000,
      cuPriceMicroLamports: 50,
    });
    
    expect(plan.cuPriceMicroLamports).toBe(100);
  });
});

describe('RecentFeesPolicy', () => {
  it('should fallback when no fees available', async () => {
    // Mock connection with empty fees
    const mockConnection = {
      getRecentPrioritizationFees: () => Promise.resolve([]),
    } as unknown as Connection;

    const policy = new RecentFeesPolicy(mockConnection, {
      minCuPrice: 100,
      maxCuLimit: 1_400_000,
    });

    const plan = await policy.suggest();
    expect(plan.cuLimit).toBe(1_400_000);
    expect(plan.cuPriceMicroLamports).toBe(100);
  });

  it('should compute percentile from recent fees', async () => {
    const mockFees = [
      { slot: 100, prioritizationFee: 100 },
      { slot: 101, prioritizationFee: 200 },
      { slot: 102, prioritizationFee: 300 },
      { slot: 103, prioritizationFee: 400 },
      { slot: 104, prioritizationFee: 500 },
    ];

    const mockConnection = {
      getRecentPrioritizationFees: () => Promise.resolve(mockFees),
    } as unknown as Connection;

    const policy = new RecentFeesPolicy(mockConnection, {
      percentile: 0.75, // p75
    });

    const plan = await policy.suggest();
    // p75 of [100, 200, 300, 400, 500] = 400
    expect(plan.cuPriceMicroLamports).toBe(400);
  });

  it('should apply EMA smoothing', async () => {
    let callCount = 0;
    const mockConnection = {
      getRecentPrioritizationFees: () => {
        callCount++;
        if (callCount === 1) {
          return Promise.resolve([{ slot: 100, prioritizationFee: 1000 }]);
        } else {
          return Promise.resolve([{ slot: 101, prioritizationFee: 500 }]);
        }
      },
    } as unknown as Connection;

    const policy = new RecentFeesPolicy(mockConnection, {
      useEma: true,
      emaAlpha: 0.5,
    });

    // First call: raw price
    const plan1 = await policy.suggest();
    expect(plan1.cuPriceMicroLamports).toBe(1000);

    // Second call: EMA kicks in
    const plan2 = await policy.suggest();
    // EMA = 0.5 * 500 + 0.5 * 1000 = 750
    expect(plan2.cuPriceMicroLamports).toBe(750);
  });

  it('should reset internal state', async () => {
    const mockConnection = {
      getRecentPrioritizationFees: () => Promise.resolve([
        { slot: 100, prioritizationFee: 1000 },
      ]),
    } as unknown as Connection;

    const policy = new RecentFeesPolicy(mockConnection, {
      useEma: true,
    });

    await policy.suggest();
    policy.reset();

    // After reset, EMA state should be cleared
    const plan = await policy.suggest();
    expect(plan.cuPriceMicroLamports).toBe(1000);
  });
});
