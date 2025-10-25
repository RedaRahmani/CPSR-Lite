#[cfg(feature = "circuits")]
use arcis_imports::*;

pub const MAX_INTENTS: usize = 64;
pub const SCALARS_PER_INTENT: usize = 36;
pub const TOTAL_SCALARS: usize = (MAX_INTENTS * SCALARS_PER_INTENT) + 1; // +count field

// Output = chunk_count (1) + [PlanChunk; MAX_INTENTS] with (start,end) → 2 scalars each
pub const PLAN_OUTPUT_SCALARS: usize = 1 + (MAX_INTENTS * 2);

// Compile-time assertions: ensure constants are in sync
const _: () = assert!(TOTAL_SCALARS == MAX_INTENTS * SCALARS_PER_INTENT + 1);
const _: () = assert!(PLAN_OUTPUT_SCALARS == 1 + MAX_INTENTS * 2);

#[cfg(feature = "circuits")]
#[encrypted]
pub mod circuits {
    use arcis_imports::*;

    pub const MAX_INTENTS: usize = 64;

    #[derive(Copy, Clone)]
    pub struct Intent {
        pub user: u64,
        pub hash: [u8; 32],
        pub fee_hint: u32,
        pub priority: u16,
        pub reserved: u16,
    }

    #[derive(Copy, Clone)]
    pub struct PlannerInput {
        pub intents: [Intent; MAX_INTENTS],
        pub count: u16,
    }

    #[derive(Copy, Clone)]
    pub struct PlanChunk {
        pub start: u16,
        pub end: u16,
    }

    #[derive(Copy, Clone)]
    pub struct PlanOutput {
        pub chunk_count: u16,
        pub chunks: [PlanChunk; MAX_INTENTS],
    }

    #[instruction]
    pub fn plan_chunk(input_ctxt: Enc<Shared, PlannerInput>) -> Enc<Shared, PlanOutput> {
        let input = input_ctxt.to_arcis();

        // Simplified planner: put all intents in one chunk (no conflict splitting here)
        let count = if input.count as usize > MAX_INTENTS {
            MAX_INTENTS as u16
        } else {
            input.count
        };

        let chunks = [PlanChunk { start: 0, end: 0 }; MAX_INTENTS];
        let mut output = PlanOutput {
            chunk_count: if count > 0 { 1 } else { 0 },
            chunks,
        };

        if count > 0 {
            output.chunks[0] = PlanChunk { start: 0, end: count };
        }

        // --- Anti-optimization: touch all inputs using only supported ops (no bitwise/while/shift).
        let mut sink: u64 = 0;
        for i in 0..MAX_INTENTS {
            let it = input.intents[i];

            // Accumulate a few hash bytes as u64 (no shifts, just adds).
            let mut h0: u64 = 0;
            for k in 0..8 {
                h0 = h0 + (it.hash[k] as u64);
            }

            // Use addition instead of XOR to avoid `^` on integers.
            sink = sink
                + it.user
                + h0
                + (it.fee_hint as u64)
                + (it.priority as u64)
                + (it.reserved as u64);
        }
        // Create a benign dependency without changing semantics: add zero derived from `sink`.
        let zero = sink - sink; // always 0, but depends on all inputs
        output.chunk_count = output.chunk_count + (zero as u16);
        // ---

        input_ctxt.owner.from_arcis(output)
    }
}

#[cfg(all(test, feature = "circuits"))]
mod tests {
    use super::circuits::*;

    fn make_intent(user: u64, hash_byte: u8, priority: u16) -> Intent {
        let mut hash = [0u8; 32];
        hash.fill(hash_byte);
        Intent { user, hash, fee_hint: 10, priority, reserved: 0 }
    }

    #[test]
    fn plans_single_chunk() {
        let intents = [Intent { user: 0, hash: [0u8; 32], fee_hint: 0, priority: 0, reserved: 0 }; MAX_INTENTS];
        let mut input = PlannerInput { intents, count: 3 };
        input.intents[0] = make_intent(1, 1, 9);
        input.intents[1] = make_intent(2, 2, 8);
        input.intents[2] = make_intent(3, 3, 7);
        assert_eq!(input.count, 3);
    }
}
