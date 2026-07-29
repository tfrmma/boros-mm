import * as grpc from '@grpc/grpc-js';
import { ApiErrorCodes, PendleContractError, ViemErrorDecoder } from '@pendle/sdk-boros';

/**
 * Maps an error thrown by the SDK into a gRPC `Status` whose `message` is
 * `"<code>: <message>"`, `rust-bridge::client::extract_code` parses that
 * exact convention back apart. A simplification instead of the full
 * `google.rpc.ErrorInfo` status-details pattern (see execution.proto's
 * module doc), revisit if this ever needs structured metadata beyond a
 * code + message.
 *
 * Two genuinely different error shapes exist here:
 *
 * 1. **Contract-level errors** (`PendleContractError`): `.errorName` is
 *    exactly one of the decoded Solidity custom error names
 *    (`MMHealthCritical`, `MarketOICapExceeded`, etc.), read directly from
 *    the SDK's own `errors/PendleContractError/type.d.ts`.
 *
 * 2. **API/REST-level errors** (the `ApiErrorCodes` family,
 *    `INSUFFICIENT_MARGIN`, `MARKET_NOT_FOUND`, etc.). The SDK itself
 *    doesn't wrap these, it uses a plain `axios` instance with no custom
 *    error class, so a rejected request throws a raw `AxiosError` and the
 *    response body is exactly whatever the Open API returns. That body
 *    shape is documented directly (`docs.pendle.finance/boros-dev/Backend/api`,
 *    "Error Handling" section):
 *    ```json
 *    { "errorCode": "INVALID_MARKET_ID", "message": "Market with ID 999 not found", "data": {} }
 *    ```
 *    (that exact code, `INVALID_MARKET_ID`, is the doc's own illustrative
 *    example and isn't actually a member of `ApiErrorCodes`, the field
 *    name (`errorCode`) is what's confirmed here, not that specific value)
 *    so the field is `errorCode`, at `err.response.data.errorCode`, not
 *    `err.code` or `err.response.data.code` as previously guessed. The
 *    Send Txs Bot service uses a different, legacy shape instead
 *    (`{ statusCode, message }`, no code field at all), so an error from
 *    that service correctly falls through to `UNKNOWN` here, there's
 *    nothing to extract. `extractApiErrorCode` checks `errorCode` first,
 *    keeps the two older guesses as fallback candidates in case a
 *    different endpoint or SDK version still uses them, and validates
 *    whatever it finds against the real `ApiErrorCodes` set before
 *    trusting it, so a wrong guess fails closed to `UNKNOWN` instead of
 *    misreading an unrelated error (a Node system error like
 *    `ECONNREFUSED`, for instance).
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

  const response = e.response as Record<string, unknown> | undefined;
  const data = response?.data as Record<string, unknown> | undefined;
  // errorCode first: confirmed real field name for the Open API's error
  // body. code/data.code kept as fallbacks only, not confirmed anywhere.
  const candidates: unknown[] = [data?.errorCode, e.code, data?.code];

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
