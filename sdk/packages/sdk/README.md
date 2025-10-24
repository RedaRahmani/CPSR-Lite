# @cpsr/sdk

TypeScript SDK for CPSR-Lite - Build, submit, and manage Solana transactions with compute budgets and Address Lookup Table support.

## Features

- 🎯 **Typed Intent Builders** - Create transactions with type-safe intent builders
- 💰 **Fee Management** - Basic and adaptive fee policies (p75, EMA, hysteresis)
- 🔗 **ALT Support** - Pluggable Address Lookup Table resolution
- 🚀 **Multiple Submission Lanes** - L1 (direct RPC) and Router (Magic Router)
- ⚡ **Compute Budget** - Automatic CU limit/price injection
- 🧪 **Well Tested** - Unit and E2E tests with solana-test-validator

## Installation

```bash
npm install @cpsr/sdk
```

## Quick Start

```typescript
import { Connection, Keypair } from '@solana/web3.js';
import { CpsrClient, transferIntent, SubmitLane } from '@cpsr/sdk';

// Initialize client
const connection = new Connection('https://api.devnet.solana.com');
const payer = Keypair.fromSecretKey(/* your secret key */);

const client = new CpsrClient({
  connection,
  payer,
  routerUrl: 'https://devnet-router.magicblock.app', // Optional
});

// Build an intent
const intent = transferIntent({
  from: payer.publicKey,
  to: recipientPublicKey,
  lamports: 1_000_000,
});

// Submit transaction
const result = await client.buildAndSubmit({
  intents: [intent],
  cuPrice: 5_000,
  cuLimit: 200_000,
  submitOptions: {
    lane: SubmitLane.L1, // or SubmitLane.Router
  },
});

console.log('Signature:', result.signature);
```

## Advanced Usage

### Using Recent Fees Policy

```typescript
import { CpsrClient } from '@cpsr/sdk';

const client = CpsrClient.withRecentFees({
  connection,
  payer,
  percentile: 0.75, // Use p75 of recent fees
  useEma: true,     // Enable EMA smoothing
  useHysteresis: true, // Avoid frequent price changes
});
```

### Building with ALT Support

```typescript
import { CpsrClient, StaticAltSource } from '@cpsr/sdk';
import { AddressLookupTableAccount } from '@solana/web3.js';

const altSource = new StaticAltSource([
  /* your ALT accounts */
]);

const client = new CpsrClient({
  connection,
  payer,
  altSource,
});
```

### Tightening Compute Units

```typescript
// Simulate to tighten CU limit before submission
const tx = await client.build({
  intents: [intent],
  cuLimit: 1_000_000,
  cuPrice: 5_000,
  tighten: true, // Simulate and adjust CU limit
});

const result = await client.submit(tx);
```

### Custom Intents

```typescript
import { customIntent } from '@cpsr/sdk';
import { TransactionInstruction } from '@solana/web3.js';

const ix = new TransactionInstruction({
  programId: yourProgramId,
  keys: [/* account metas */],
  data: Buffer.from([/* instruction data */]),
});

const intent = customIntent({
  actor: payer.publicKey,
  instruction: ix,
  options: { priority: 10 },
});
```

## API Documentation

Full API documentation is available in the [docs/](./docs/) directory after building:

```bash
npm run docs
```

## Development

```bash
# Install dependencies
npm install

# Build
npm run build

# Run tests
npm test

# Run unit tests only
npm run test:unit

# Run E2E tests (requires solana-test-validator)
npm run test:e2e

# Generate docs
npm run docs

# Lint
npm run lint
```

## Testing

### Unit Tests

Unit tests cover core functionality without external dependencies:

```bash
npm run test:unit
```

### E2E Tests

E2E tests require `solana-test-validator` to be installed and available in PATH:

```bash
npm run test:e2e
```

The E2E tests include:
- Basic transfer submission
- Memo instruction handling
- Hot account scenarios with retries
- Compute unit tightening

## Architecture

The SDK is structured into modules:

- **intents/** - Intent types and builders
- **tx/** - Transaction building, fee policies, ALT resolution
- **submit/** - Submission strategies (L1, Router)
- **errors.ts** - Typed error classes
- **client.ts** - Main unified client

## Error Handling

The SDK provides typed errors for different failure scenarios:

```typescript
import {
  BlockhashStaleError,
  PreflightFailedError,
  RpcRetriableError,
  TransactionTooLargeError,
  RouterJsonRpcError,
  NetworkError,
} from '@cpsr/sdk';

try {
  await client.buildAndSubmit({ /* ... */ });
} catch (error) {
  if (error instanceof BlockhashStaleError) {
    // Retry with fresh blockhash
  } else if (error instanceof RpcRetriableError) {
    // Backoff and retry
  } else if (error instanceof PreflightFailedError) {
    // Transaction would fail, investigate logs
    console.error('Preflight logs:', error.logs);
  }
}
```

## Publishing

This package uses npm's trusted publishing via GitHub Actions OIDC. To publish:

1. Update version in `package.json`
2. Create and push a git tag:
   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```
3. GitHub Actions will automatically build, test, and publish to npm

## Contributing

Contributions are welcome! Please ensure:
- All tests pass (`npm test`)
- Code is linted (`npm run lint`)
- Documentation is updated (`npm run docs`)

## License

MIT

## Links

- [CPSR-Lite Repository](https://github.com/RedaRahmani/CPSR-Lite)
- [Solana Documentation](https://docs.solana.com)
- [Magic Router Documentation](https://docs.magicblock.gg)
