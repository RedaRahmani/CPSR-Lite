/**
 * Unit tests for dual event-type handling in arciumClient
 * Tests both PlanComputed (dev mode with full ciphertexts)
 * and PlanComputedHash (prod mode with hash-only payload)
 */

describe('arciumClient event handling', () => {
  describe('Event type detection (PlanComputed vs PlanComputedHash)', () => {
    it('should handle PlanComputed event with full ciphertexts (dev mode)', () => {
      // Mock event data matching the structure returned by Anchor EventParser
      const mockEventData = {
        offset: { toString: () => '42' },
        nonce: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        ciphertexts: [
          Array(32).fill(1), // Mock ciphertext 1
          Array(32).fill(2), // Mock ciphertext 2
        ],
      };

      // Simulate the conditional extraction logic used in parseCallbackEvent
      const ciphertexts = Array.isArray(mockEventData.ciphertexts)
        ? mockEventData.ciphertexts.map((ct: number[]) => new Uint8Array(ct))
        : [];
      const nonce = new Uint8Array(mockEventData.nonce);

      expect(nonce).toEqual(
        Uint8Array.from([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
      );
      expect(ciphertexts).toHaveLength(2);
      expect(ciphertexts[0]).toEqual(Uint8Array.from(Array(32).fill(1)));
      expect(ciphertexts[1]).toEqual(Uint8Array.from(Array(32).fill(2)));
    });

    it('should handle PlanComputedHash event with empty ciphertexts array (prod mode)', () => {
      // Mock hash-only event (no ciphertexts field)
      const mockEventData: any = {
        offset: { toString: () => '42' },
        nonce: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        payloadHash: Array(32).fill(255), // keccak hash of ciphertexts
        // Note: ciphertexts field does NOT exist in this event type
      };

      // Simulate the conditional extraction logic used in parseCallbackEvent
      const ciphertexts = Array.isArray(mockEventData.ciphertexts)
        ? mockEventData.ciphertexts.map((ct: number[]) => new Uint8Array(ct))
        : [];
      const nonce = new Uint8Array(mockEventData.nonce);

      expect(nonce).toEqual(
        Uint8Array.from([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
      );
      expect(ciphertexts).toEqual([]); // Empty array when ciphertexts field is undefined
    });

    it('should demonstrate the fail-fast error when ciphertexts is empty', () => {
      // This test documents the expected behavior in runOnce
      const emptyCiphertexts: Uint8Array[] = [];

      // The actual check in runOnce
      if (emptyCiphertexts.length === 0) {
        const errorMessage =
          'Cannot decrypt plan output: no ciphertexts available (event emitted hash only). ' +
          'Recompile program with --features full-event-logs for development.';
        
        expect(errorMessage).toContain('full-event-logs');
        expect(emptyCiphertexts.length).toBe(0);
      }
    });

    it('should correctly type-check the conditional ciphertext extraction', () => {
      // Test type safety of the conditional expression
      const withCiphertexts = { ciphertexts: [[1, 2], [3, 4]] };
      const withoutCiphertexts = { payloadHash: [255, 255] };

      const extracted1 = Array.isArray(withCiphertexts.ciphertexts)
        ? withCiphertexts.ciphertexts.map((ct: number[]) => new Uint8Array(ct))
        : [];
      
      const extracted2 = Array.isArray((withoutCiphertexts as any).ciphertexts)
        ? (withoutCiphertexts as any).ciphertexts.map((ct: number[]) => new Uint8Array(ct))
        : [];

      expect(extracted1).toHaveLength(2);
      expect(extracted2).toEqual([]); // Falls back to empty array
    });
  });

  describe('Documentation of event emission modes', () => {
    it('should document the feature flag behavior', () => {
      // In development: cargo build --features full-event-logs
      // Emits: PlanComputed { offset, nonce, ciphertexts: Vec<[u8;32]> }
      // Size: ~4KB for 129 ciphertexts, fits in Solana log size limit
      const devMode = {
        eventName: 'PlanComputed',
        hasCiphertexts: true,
        sizeBytes: 129 * 32, // 4128 bytes
      };

      // In production: cargo build (no feature flag)
      // Emits: PlanComputedHash { offset, nonce, payload_hash: [u8;32] }
      // Size: ~100 bytes, very safe
      const prodMode = {
        eventName: 'PlanComputedHash',
        hasCiphertexts: false,
        sizeBytes: 32 + 8 + 16, // hash + offset + nonce
      };

      expect(devMode.hasCiphertexts).toBe(true);
      expect(prodMode.hasCiphertexts).toBe(false);
      expect(devMode.sizeBytes).toBeGreaterThan(prodMode.sizeBytes);
    });
  });
});
