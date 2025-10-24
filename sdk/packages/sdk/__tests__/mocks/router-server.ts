/**
 * Mock Router server for testing
 * 
 * Provides endpoints:
 * - getRoutes
 * - simulateTransaction
 * - sendTransaction
 * 
 * Supports behavior toggles for testing error conditions
 */

import { createServer, Server, IncomingMessage, ServerResponse } from 'http';

export interface MockRouterBehavior {
  /** Return OK response */
  mode: 'ok' | 'preflight-fail' | 'rate-limit' | 'server-error';
  /** Latency to inject (ms) */
  latency?: number;
  /** Custom error message */
  errorMessage?: string;
  /** Custom error code */
  errorCode?: number;
}

export class MockRouterServer {
  private server: Server | null = null;
  private port: number;
  public behavior: MockRouterBehavior;

  constructor(port: number = 8765) {
    this.port = port;
    this.behavior = { mode: 'ok' };
  }

  start(): Promise<void> {
    return new Promise((resolve, reject) => {
      this.server = createServer((req, res) => this.handleRequest(req, res));

      this.server.on('error', reject);

      this.server.listen(this.port, () => {
        resolve();
      });
    });
  }

  async stop(): Promise<void> {
    return new Promise((resolve, reject) => {
      if (this.server) {
        this.server.close((err) => {
          if (err) reject(err);
          else resolve();
        });
      } else {
        resolve();
      }
    });
  }

  getUrl(): string {
    return `http://localhost:${this.port}`;
  }

  setBehavior(behavior: MockRouterBehavior): void {
    this.behavior = behavior;
  }

  private async handleRequest(
    req: IncomingMessage,
    res: ServerResponse
  ): Promise<void> {
    // Inject latency if configured
    if (this.behavior.latency && this.behavior.latency > 0) {
      await new Promise((resolve) => setTimeout(resolve, this.behavior.latency));
    }

    // Parse body
    let body = '';
    for await (const chunk of req) {
      body += chunk.toString();
    }

    let jsonRpcRequest: {
      jsonrpc: string;
      id: number;
      method: string;
      params: unknown[];
    };

    try {
      jsonRpcRequest = JSON.parse(body);
    } catch {
      res.writeHead(400, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: 'Invalid JSON' }));
      return;
    }

    const { method, id } = jsonRpcRequest;

    // Handle different modes
    if (this.behavior.mode === 'rate-limit') {
      res.writeHead(429, { 'Content-Type': 'application/json' });
      res.end(
        JSON.stringify({
          jsonrpc: '2.0',
          id,
          error: {
            code: -32005,
            message: this.behavior.errorMessage || 'Rate limit exceeded',
          },
        })
      );
      return;
    }

    if (this.behavior.mode === 'server-error') {
      res.writeHead(503, { 'Content-Type': 'application/json' });
      res.end(
        JSON.stringify({
          jsonrpc: '2.0',
          id,
          error: {
            code: this.behavior.errorCode || -32603,
            message: this.behavior.errorMessage || 'Internal server error',
          },
        })
      );
      return;
    }

    // Route by method
    switch (method) {
      case 'getRoutes':
        this.handleGetRoutes(res, id);
        break;
      case 'simulateTransaction':
        this.handleSimulateTransaction(res, id);
        break;
      case 'sendTransaction':
        this.handleSendTransaction(res, id);
        break;
      default:
        res.writeHead(400, { 'Content-Type': 'application/json' });
        res.end(
          JSON.stringify({
            jsonrpc: '2.0',
            id,
            error: {
              code: -32601,
              message: `Method not found: ${method}`,
            },
          })
        );
    }
  }

  private handleGetRoutes(res: ServerResponse, id: number): void {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(
      JSON.stringify({
        jsonrpc: '2.0',
        id,
        result: {
          endpoint: 'http://mock-route.example.com',
          ttl: 300,
        },
      })
    );
  }

  private handleSimulateTransaction(res: ServerResponse, id: number): void {
    if (this.behavior.mode === 'preflight-fail') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(
        JSON.stringify({
          jsonrpc: '2.0',
          id,
          result: {
            err: {
              InstructionError: [0, 'Custom program error: 0x1'],
            },
            logs: ['Program log: Simulation failed'],
          },
        })
      );
      return;
    }

    // OK response
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(
      JSON.stringify({
        jsonrpc: '2.0',
        id,
        result: {
          err: null,
          logs: ['Program log: Success'],
          unitsConsumed: 5000,
        },
      })
    );
  }

  private handleSendTransaction(res: ServerResponse, id: number): void {
    if (this.behavior.mode === 'preflight-fail') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(
        JSON.stringify({
          jsonrpc: '2.0',
          id,
          error: {
            code: -32002,
            message: 'Transaction simulation failed',
          },
        })
      );
      return;
    }

    // OK response
    const mockSignature =
      '5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQUW';
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(
      JSON.stringify({
        jsonrpc: '2.0',
        id,
        result: mockSignature,
      })
    );
  }
}
