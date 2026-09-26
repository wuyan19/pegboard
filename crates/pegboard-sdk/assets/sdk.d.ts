// pegboard sdk.d.ts — M3：store 类型。files / fetch / connectWS / url 随 M4-M6 提供。
declare namespace Pegboard {
  interface HostError extends Error {
    code:
      | "APP_NOT_FOUND"
      | "PERMISSION_DENIED"
      | "TARGET_DENIED"
      | "LIMIT_EXCEEDED"
      | "NOT_FOUND"
      | "INVALID_REQUEST"
      | "TOKEN_INVALID"
      | "UPSTREAM_ERROR"
      | "TIMEOUT";
    detail?: Record<string, unknown>;
  }

  interface KVEntry {
    key: string;
    value: unknown;
  }

  interface Store {
    get(key: string): Promise<unknown | null>;
    set(key: string, value: unknown): Promise<void>;
    delete(key: string): Promise<void>;
    list(prefix?: string): Promise<KVEntry[]>;
    batch(ops: { op: "set" | "delete"; key: string; value?: unknown }[]): Promise<void>;
  }

  interface Host {
    store: Store;
    __pegboard?: { appId: string | null; shim: boolean };
  }
}

declare global {
  interface Window {
    host: Pegboard.Host;
    __PEGBOARD__?: { appId: string; shim: boolean };
  }
}

export {};
