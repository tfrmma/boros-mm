import * as grpc from '@grpc/grpc-js';
import { ApiErrorCodes, PendleContractError, ViemErrorDecoder } from '@pendle/sdk-boros';

/**
 * Maps an error thrown by the SDK into a gRPC `Status` whose `message` is
 * `"<code>: <message>"`, `rust-bridge::client::extract_code` parses that
 * exact convention back apart. A deliberate simplification instead of the
 * full `google.rpc.ErrorInfo` status-details pattern (see execution.proto's
 * module doc), revisit if this ever needs structured metadata beyond a
 * code + message.
 *
 * Two genuinely different error shapes exist here, with very different
 * confidence levels:
 *
 * 1. **Contract-level errors** (`PendleContractError`), VERIFIED against
 *    the SDK's own exported class: `.errorName` is exactly one of the
 *    decoded Solidity custom error names (`MMHealthCritical`,
 *    `MarketOICapExceeded`, etc.), read directly from
 *    `errors/PendleContractError/type.d.ts`.
 *
 * 2. **API/REST-level errors** (the `ApiErrorCodes` family,
 *    `INSUFFICIENT_MARGIN`, `MARKET_NOT_FOUND`, etc.). The set of valid
 *    codes IS verified (`ApiErrorCodes` is a real export, confirmed
 *    2026-07-18 via `dist/errors/ErrorCodes.d.ts` in the compiled
 *    package). What's still not fully confirmed is the exact shape of the
 *    thrown error object for this specific family. Partial evidence found
 *    2026-07-19: the SDK does use `axios` as its HTTP client (confirmed
 *    via `import { AxiosError } from 'axios'` in
 *    `AggregatorHelperErrors.d.ts`, a different subsystem but the same
 *    package), which supports the general `err.response.data.X` shape
 *    already assumed here. What's NOT confirmed: the exact field name
 *    Boros's trading API puts the code under (`code`? `errorCode`?
 *    something else?), none of the `.d.ts` files or the compiled JS
 *    expose a consuming site for `ApiErrorCodes` that would show it.
 *    `extractApiErrorCode` below tries the shapes a REST client thrown
 *    error would plausibly have (`err.code`, `err.response.data.code`),
 *    then checks the result against the real `ApiErrorCodes` set before
 *    trusting it, so a wrong guess fails closed to `UNKNOWN` instead of
 *    misreading an unrelated error (a Node system error like
 *    `ECONNREFUSED`, for instance). If real traffic shows the field name
 *    itself is wrong, fix it against an actual observed error, not by
 *    guessing harder.
 */
export function toGrpcError(err: unknown): grpc.ServiceError {
  const decoded = err instanceof Error ? ViemErrorDecoder.decodeViemError(err) : err;

  if (decoded instanceof PendleContractError) {
    return statusOf(decoded.errorName, decoded.message);
  }

  const apiCode = extractApiErrorCode(err);
  if (apiCode) {
    return statusOf(apiCode, errorMessage(err));
  }

  return statusOf('UNKNOWN', errorMessage(err));
}

const KNOWN_API_ERROR_CODES: ReadonlySet<string> = new Set(Object.values(ApiErrorCodes));

function extractApiErrorCode(err: unknown): string | undefined {
  if (typeof err !== 'object' || err === null) return undefined;
  const e = err as Record<string, unknown>;

  const candidates: unknown[] = [e.code];
  const response = e.response as Record<string, unknown> | undefined;
  const data = response?.data as Record<string, unknown> | undefined;
  candidates.push(data?.code);

  for (const candidate of candidates) {
    if (typeof candidate === 'string' && KNOWN_API_ERROR_CODES.has(candidate)) {
      return candidate;
    }
  }
  return undefined;
}

function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
}

function statusOf(code: string, message: string): grpc.ServiceError {
  const status = grpc.status.UNKNOWN; // transport-level code; rust-bridge classifies retriability from the embedded code, not this
  return Object.assign(new Error(`${code}: ${message}`), { code: status }) as grpc.ServiceError;
}
