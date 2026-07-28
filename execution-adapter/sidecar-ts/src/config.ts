/**
 * All configuration comes from the environment, nothing about which
 * market, RPC endpoint, or listen address this process uses is hardcoded,
 * matching this workspace's "nothing hardcoded that doesn't have to be"
 * principle (see README's design principles).
 */

function required(name: string): string {
  const v = process.env[name];
  if (!v) throw new Error(`missing required env var: ${name}`);
  return v;
}

export interface SidecarConfig {
  /** Address this gRPC server listens on, e.g. "0.0.0.0:50051". */
  listenAddr: string;
  /** RPC URLs handed to the SDK's Exchange constructor (comma-separated). */
  rpcUrls: string[];
  /** The root account's on-chain address. */
  root: `0x${string}`;
  /** Sub-account id under `root` (Exchange constructor's `accountId`). */
  accountId: number;
}

export function loadConfig(): SidecarConfig {
  return {
    listenAddr: process.env.SIDECAR_LISTEN_ADDR ?? '0.0.0.0:50051',
    rpcUrls: required('BOROS_RPC_URLS').split(',').map((s) => s.trim()),
    root: required('BOROS_ROOT_ADDRESS') as `0x${string}`,
    accountId: Number(process.env.BOROS_ACCOUNT_ID ?? '0'),
  };
}
