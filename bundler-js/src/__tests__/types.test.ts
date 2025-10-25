/**
 * Unit tests for MPC type packing/unpacking
 */

import {
  packIntent,
  packPlannerInput,
  unpackPlanOutput,
  MAX_INTENTS,
  Intent,
  PlannerInput,
} from '../types';

describe('packIntent', () => {
  it('should pack a valid intent into 36 scalars', () => {
    const intent: Intent = {
      user: 123n,
      hash: new Uint8Array(32).fill(0xAB),
      feeHint: 1000,
      priority: 5,
      reserved: 0,
    };

    const fields = packIntent(intent);
    
    expect(fields.length).toBe(36);
    expect(fields[0]).toBe(123n); // user
    expect(fields[1]).toBe(0xABn); // hash[0]
    expect(fields[32]).toBe(0xABn); // hash[31]
    expect(fields[33]).toBe(1000n); // feeHint
    expect(fields[34]).toBe(5n); // priority
    expect(fields[35]).toBe(0n); // reserved
  });

  it('should reject intent with user > u64::MAX', () => {
    const intent: Intent = {
      user: 0xFFFFFFFFFFFFFFFFn + 1n, // u64::MAX + 1
      hash: new Uint8Array(32),
      feeHint: 0,
      priority: 0,
      reserved: 0,
    };

    expect(() => packIntent(intent)).toThrow('exceeds u64::MAX');
  });

  it('should reject intent with feeHint > u32::MAX', () => {
    const intent: Intent = {
      user: 0n,
      hash: new Uint8Array(32),
      feeHint: 0xFFFFFFFF + 1, // u32::MAX + 1
      priority: 0,
      reserved: 0,
    };

    expect(() => packIntent(intent)).toThrow('exceeds u32::MAX');
  });

  it('should reject intent with priority > u16::MAX', () => {
    const intent: Intent = {
      user: 0n,
      hash: new Uint8Array(32),
      feeHint: 0,
      priority: 0xFFFF + 1, // u16::MAX + 1
      reserved: 0,
    };

    expect(() => packIntent(intent)).toThrow('exceeds u16::MAX');
  });

  it('should reject intent with invalid hash length', () => {
    const intent: Intent = {
      user: 0n,
      hash: new Uint8Array(31), // should be 32
      feeHint: 0,
      priority: 0,
      reserved: 0,
    };

    expect(() => packIntent(intent)).toThrow('must be exactly 32 bytes');
  });
});

describe('packPlannerInput', () => {
  const makeIntent = (user: number): Intent => ({
    user: BigInt(user),
    hash: new Uint8Array(32).fill(user),
    feeHint: 1000,
    priority: 5,
    reserved: 0,
  });

  it('should pack empty input (count=0) into 2305 scalars', () => {
    const input: PlannerInput = {
      intents: [],
      count: 0,
    };

    const fields = packPlannerInput(input);
    
    // 64 intents * 36 + 1 count = 2305
    expect(fields.length).toBe(2305);
    expect(fields[2304]).toBe(0n); // count field at the end
  });

  it('should pack single intent (count=1) into 2305 scalars', () => {
    const input: PlannerInput = {
      intents: [makeIntent(1)],
      count: 1,
    };

    const fields = packPlannerInput(input);
    
    expect(fields.length).toBe(2305);
    expect(fields[0]).toBe(1n); // first intent user
    expect(fields[2304]).toBe(1n); // count field
  });

  it('should pack MAX_INTENTS into 2305 scalars', () => {
    const intents = Array.from({ length: MAX_INTENTS }, (_, i) => makeIntent(i));
    const input: PlannerInput = {
      intents,
      count: MAX_INTENTS,
    };

    const fields = packPlannerInput(input);
    
    expect(fields.length).toBe(2305);
    expect(fields[2304]).toBe(BigInt(MAX_INTENTS)); // count field
  });

  it('should pad intents to MAX_INTENTS when count < MAX_INTENTS', () => {
    const input: PlannerInput = {
      intents: [makeIntent(1), makeIntent(2)],
      count: 2,
    };

    const fields = packPlannerInput(input);
    
    expect(fields.length).toBe(2305);
    // Verify first two intents
    expect(fields[0]).toBe(1n);
    expect(fields[36]).toBe(2n);
    // Verify padding (third intent should be zeros)
    expect(fields[72]).toBe(0n);
  });

  it('should reject count > MAX_INTENTS', () => {
    const input: PlannerInput = {
      intents: [],
      count: MAX_INTENTS + 1,
    };

    expect(() => packPlannerInput(input)).toThrow('exceeds MAX_INTENTS');
  });

  it('should reject too many intents in array', () => {
    const intents = Array.from({ length: MAX_INTENTS + 1 }, (_, i) => makeIntent(i));
    const input: PlannerInput = {
      intents,
      count: MAX_INTENTS + 1,
    };

    expect(() => packPlannerInput(input)).toThrow('exceeds MAX_INTENTS');
  });
});

describe('unpackPlanOutput', () => {
  it('should accept exactly 129 fields', () => {
    const validFields = new Array(129).fill(0n);
    
    const result = unpackPlanOutput(validFields);
    
    expect(result.chunkCount).toBe(0);
    expect(result.chunks).toEqual([]);
  });

  it('should unpack chunk_count and chunks correctly', () => {
    const fields = new Array(129).fill(0n);
    fields[0] = 2n; // chunk_count
    fields[1] = 0n; // chunk[0].start
    fields[2] = 5n; // chunk[0].end
    fields[3] = 5n; // chunk[1].start
    fields[4] = 10n; // chunk[1].end
    
    const result = unpackPlanOutput(fields);
    
    expect(result.chunkCount).toBe(2);
    expect(result.chunks.length).toBe(2);
    expect(result.chunks[0]).toEqual({ start: 0, end: 5 });
    expect(result.chunks[1]).toEqual({ start: 5, end: 10 });
  });

  it('should reject 128 fields (off-by-one low)', () => {
    const invalidFields = new Array(128).fill(0n);
    
    expect(() => unpackPlanOutput(invalidFields)).toThrow('Expected 129 fields, got 128');
  });

  it('should reject 130 fields (off-by-one high)', () => {
    const invalidFields = new Array(130).fill(0n);
    
    expect(() => unpackPlanOutput(invalidFields)).toThrow('Expected 129 fields, got 130');
  });

  it('should reject 193 fields (old incorrect expectation)', () => {
    const invalidFields = new Array(193).fill(0n);
    
    expect(() => unpackPlanOutput(invalidFields)).toThrow('Expected 129 fields, got 193');
  });

  it('should slice chunks array to chunkCount', () => {
    const fields = new Array(129).fill(0n);
    fields[0] = 1n; // chunk_count = 1
    fields[1] = 0n; // chunk[0].start
    fields[2] = 5n; // chunk[0].end
    // Remaining chunks are zeros (padding)
    
    const result = unpackPlanOutput(fields);
    
    expect(result.chunks.length).toBe(1); // Only first chunk
    expect(result.chunks[0]).toEqual({ start: 0, end: 5 });
  });
});

describe('nonce round-trip', () => {
  // Import the nonce functions (they're in encrypt.ts)
  const { generateNonce, nonceToBytes } = require('../encrypt');

  it('should produce 16-byte LE representation from nonce', () => {
    const nonce = 0x0123456789ABCDEFn; // 64-bit value
    
    const bytes = nonceToBytes(nonce);
    
    expect(bytes.length).toBe(16);
    // Little-endian: byte 0 = LSB
    expect(bytes[0]).toBe(0xEF);
    expect(bytes[1]).toBe(0xCD);
    expect(bytes[2]).toBe(0xAB);
    expect(bytes[3]).toBe(0x89);
    expect(bytes[4]).toBe(0x67);
    expect(bytes[5]).toBe(0x45);
    expect(bytes[6]).toBe(0x23);
    expect(bytes[7]).toBe(0x01);
    // Upper 8 bytes should be zero
    expect(bytes[8]).toBe(0x00);
    expect(bytes[15]).toBe(0x00);
  });

  it('should reconstruct nonce from bytes (LE round-trip)', () => {
    const original = 0x123456789ABCDEFn;
    
    const bytes = nonceToBytes(original);
    
    // Manually reconstruct to verify LE
    let reconstructed = 0n;
    for (let i = 0; i < 16; i++) {
      reconstructed |= BigInt(bytes[i]) << BigInt(i * 8);
    }
    
    expect(reconstructed).toBe(original);
  });

  it('should handle full 128-bit nonce', () => {
    const nonce = (1n << 127n) | 0x0123456789ABCDEFn; // MSB set + some lower bits
    
    const bytes = nonceToBytes(nonce);
    
    expect(bytes.length).toBe(16);
    expect(bytes[15]).toBe(0x80); // MSB byte
  });
});
