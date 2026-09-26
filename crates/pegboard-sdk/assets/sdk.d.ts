// pegboard sdk.d.ts — 占位版本（M2）。完整类型随 M3-M6 提供。
declare namespace Pegboard {
  interface Host {
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
