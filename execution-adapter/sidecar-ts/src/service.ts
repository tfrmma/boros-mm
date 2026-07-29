import * as grpc from '@grpc/grpc-js';
import { Exchange, Agent, Side, TimeInForce, BorosBackend } from '@pendle/sdk-boros';
import { createWalletClient, http, WalletClient, Hex } from 'viem';
import { privateKeyToAccount } from 'viem/accounts';

import { SidecarConfig } from './config';
import { toGrpcError } from './errorMapping';

/**
 * proto Side is shifted by +1 from the SDK's Side (proto3 reserves 0 for
 * "unspecified", see execution.proto's module doc). TimeInForce likewise.
 */
function sideFromProto(protoSide: number): Side {
  if (protoSide === 1) return Side.LONG;
  if (protoSide === 2) return Side.SHORT;
  throw new Error(`unspecified or unknown proto Side: ${protoSide}`);
}

function tifFromProto(protoTif: number): TimeInForce {
  switch (protoTif) {
    case 1: return TimeInForce.GOOD_TIL_CANCELLED;
    case 2: return TimeInForce.IMMEDIATE_OR_CANCEL;
    case 3: return TimeInForce.FILL_OR_KILL;
    case 4: return TimeInForce.ADD_LIQUIDITY_ONLY;
    case 5: return TimeInForce.SOFT_ADD_LIQUIDITY_ONLY;
    default: throw new Error(`unspecified or unknown proto TimeInForce: ${protoTif}`);
  }
}

/** FixedX18 crosses the wire as a decimal string of the raw i128/bigint,
 * see execution.proto's module doc. The SDK's own `size` field is a plain
 * `bigint` (not FixedX18-wrapped), so this is a straight string<->bigint
 * conversion, not a rescale.
 */
function fixedStringToBigint(s: string): bigint {
  return BigInt(s);
}

export class ExecutionServiceImpl {
  private exchange: Exchange;
  private statusSdk: BorosBackend.BorosSendTxsBotSdk;

  private constructor(exchange: Exchange, statusSdk: BorosBackend.BorosSendTxsBotSdk) {
    this.exchange = exchange;
    this.statusSdk = statusSdk;
  }

  /**
   * `agentPrivateKey` is already resolved by the time it gets here, see
   * `secrets.ts`'s `SecretProvider`, this constructor doesn't care whether
   * it came from an env var (local dev) or AWS Secrets Manager
   * (production). The root wallet's private key is NOT a
   * parameter here at all: per this workspace's key-separation design, the
   * root wallet only ever signs the one-time agent approval, offline,
   * outside this process entirely. This sidecar only ever holds the agent
   * key.
   */
  static async create(config: SidecarConfig, agentPrivateKey: Hex, apiBaseUrl: string): Promise<ExecutionServiceImpl> {
    const account = privateKeyToAccount(agentPrivateKey);
    const walletClient: WalletClient = createWalletClient({
      account,
      transport: http(config.rpcUrls[0]),
    });

    const agent = Agent.createFromPrivateKey(agentPrivateKey);
    const exchange = new Exchange(walletClient, config.root, config.accountId, config.rpcUrls, agent);
    const statusSdk = BorosBackend.createSendTxsBotSdk(apiBaseUrl);

    return new ExecutionServiceImpl(exchange, statusSdk);
  }

  /** Test-only escape hatch, `create()` above is the real entry point and
   * does real network setup (wallet client, live `Exchange`). This just
   * gives tests a way to construct the service against fakes without
   * dragging viem/a real RPC into the test run. */
  static createForTesting(exchange: Exchange, statusSdk: BorosBackend.BorosSendTxsBotSdk): ExecutionServiceImpl {
    return new ExecutionServiceImpl(exchange, statusSdk);
  }

  async placeOrder(call: grpc.ServerUnaryCall<any, any>, callback: grpc.sendUnaryData<any>): Promise<void> {
    try {
      const req = call.request;
      const result = await this.exchange.placeOrder({
        marketAcc: req.marketAcc as Hex,
        marketId: req.marketId,
        side: sideFromProto(req.side),
        size: fixedStringToBigint(req.size),
        limitTick: req.limitTick ?? undefined,
        slippage: req.slippage ?? undefined,
        tif: tifFromProto(req.tif),
      });

      callback(null, {
        txHash: result.executeResponse.txHash ?? '',
        orderId: result.result.order?.orderId ? String(result.result.order.orderId) : undefined,
        // Fixed 2026-07-18: filledSize comes back as a FixedX18 object, not
        // a bigint (verified against @pendle/sdk-boros@1.5.0's exchange.d.ts
        // and @pendle/boros-offchain-math@1.0.6's FixedX18.toString(), which
        // formats as "1234.500000000000000000", a decimal string that
        // rust-bridge's fixed_from_string() can't parse as i128. .value is
        // the raw scaled bigint underneath, that's what actually matches
        // the wire convention. placedSize below is a real bigint already,
        // it never had this problem.
        filledSize: String(result.result.order?.filledSize?.value ?? 0n),
        placedSize: result.result.order?.placedSize !== undefined ? String(result.result.order.placedSize) : undefined,
        status: statusToProto(result.executeResponse.status),
      });
    } catch (err) {
      callback(toGrpcError(err), null);
    }
  }

  async cancelOrders(call: grpc.ServerUnaryCall<any, any>, callback: grpc.sendUnaryData<any>): Promise<void> {
    try {
      const req = call.request;
      const result = await this.exchange.cancelOrders({
        marketAcc: req.marketAcc as Hex,
        marketId: req.marketId,
        cancelAll: req.cancelAll,
        orderIds: req.orderIds as string[],
      });

      callback(null, {
        txHash: result.executeResponse.txHash ?? '',
        status: statusToProto(result.executeResponse.status),
      });
    } catch (err) {
      callback(toGrpcError(err), null);
    }
  }

  async getTxStatus(call: grpc.ServerUnaryCall<any, any>, callback: grpc.sendUnaryData<any>): Promise<void> {
    try {
      const req = call.request;
      const resp = await this.statusSdk.agent.agentControllerTrace({ agent: req.agent, nonce: req.nonce });

      callback(null, {
        submissionStatus: traceStatusToProto(resp.data.submissionStatus),
        nonceStatus: traceStatusToProto(resp.data.nonceStatus),
        txHash: resp.data.txHash,
      });
    } catch (err) {
      callback(toGrpcError(err), null);
    }
  }
}

// proto TxStatus: 0=UNSPECIFIED, 1=HAVENT_SEEN, 2=PROCESSING, 3=PROCESSED, 4=SEND_FAILED
function traceStatusToProto(s: 'HAVENT_SEEN' | 'PROCESSING' | 'PROCESSED' | 'SEND_FAILED'): number {
  switch (s) {
    case 'HAVENT_SEEN': return 1;
    case 'PROCESSING': return 2;
    case 'PROCESSED': return 3;
    case 'SEND_FAILED': return 4;
  }
}

/**
 * `executeResponse.status` isn't precisely typed in the `.d.ts` this was
 * verified against (`TxResponse.status?: string`), best-effort mapping of
 * the plausible string values onto the same TxStatus enum used for trace
 * polling above. Flagged the same way as the API error code extraction:
 * fix against an observed real value if this guesses wrong, not by
 * guessing harder.
 */
function statusToProto(status: string | undefined): number {
  switch (status) {
    case 'PROCESSED': return 3;
    case 'PROCESSING': return 2;
    case 'SEND_FAILED': return 4;
    default: return 1; // HAVENT_SEEN as the conservative default for an unrecognized/absent status
  }
}
