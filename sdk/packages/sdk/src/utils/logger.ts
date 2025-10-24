/**
 * Simple logger utility
 * @module utils/logger
 * @internal
 */

export enum LogLevel {
  DEBUG = 0,
  INFO = 1,
  WARN = 2,
  ERROR = 3,
  NONE = 4,
}

export class Logger {
  constructor(
    private readonly level: LogLevel = LogLevel.INFO,
    private readonly name: string = 'cpsr'
  ) {}

  debug(message: string, ...args: unknown[]): void {
    if (this.level <= LogLevel.DEBUG) {
      console.debug(`[${this.name}:DEBUG]`, message, ...args);
    }
  }

  info(message: string, ...args: unknown[]): void {
    if (this.level <= LogLevel.INFO) {
      console.info(`[${this.name}:INFO]`, message, ...args);
    }
  }

  warn(message: string, ...args: unknown[]): void {
    if (this.level <= LogLevel.WARN) {
      console.warn(`[${this.name}:WARN]`, message, ...args);
    }
  }

  error(message: string, ...args: unknown[]): void {
    if (this.level <= LogLevel.ERROR) {
      console.error(`[${this.name}:ERROR]`, message, ...args);
    }
  }
}
