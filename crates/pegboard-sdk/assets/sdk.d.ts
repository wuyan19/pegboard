// pegboard sdk.d.ts — M5：store / files / fetch / url 类型。connectWS 随 M6 提供。
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

  interface FileMeta {
    id: string;
    name: string;
    size: number;
    mime: string;
    created: number;
  }

  interface Files {
    upload(file: Blob | File): Promise<Omit<FileMeta, "created">>;
    get(id: string): Promise<Blob | null>;
    url(id: string): string;
    sign(id: string, ttlSec: number): Promise<string>;
    list(opts?: { prefix?: string; cursor?: string; limit?: number }): Promise<{
      items: FileMeta[];
      next: string | null;
    }>;
    delete(id: string): Promise<void>;
  }

  interface Host {
    store: Store;
    files: Files;
    fetch(url: string, options?: RequestInit): Promise<Response>;
    url(url: string): string;
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
