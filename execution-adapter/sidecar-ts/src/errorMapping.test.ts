import { test } from 'node:test';
import assert from 'node:assert/strict';
import { PendleContractError } from '@pendle/sdk-boros';
import { toGrpcError } from './errorMapping';

test('PendleContractError maps to "<errorName>: <message>"', () => {
  const cause = new Error('reverted');
  const contractErr = new PendleContractError('MMHealthCritical', [] as any, cause);
  const grpcErr = toGrpcError(contractErr);
  assert.match(grpcErr.message, /^MMHealthCritical: /);
});

test('plain object with .code is extracted as the API error code', () => {
  const apiErr = { code: 'INSUFFICIENT_MARGIN', message: 'not enough margin' };
  const grpcErr = toGrpcError(apiErr);
  assert.match(grpcErr.message, /^INSUFFICIENT_MARGIN: /);
});

test('axios-shaped error (response.data.code) is extracted', () => {
  const axiosErr = { response: { data: { code: 'MARKET_NOT_FOUND' } }, message: 'Request failed' };
  const grpcErr = toGrpcError(axiosErr);
  assert.match(grpcErr.message, /^MARKET_NOT_FOUND: /);
});

test('the real Open API error shape (response.data.errorCode) is extracted', () => {
  // shape confirmed against docs.pendle.finance/boros-dev/Backend/api's
  // Error Handling section: { errorCode, message, data }. The docs'
  // own example code, INVALID_MARKET_ID, isn't actually a member of
  // ApiErrorCodes though, just an illustrative name, so this uses a real
  // one (MARKET_NOT_FOUND) to test the field-name extraction honestly.
  const openApiErr = { response: { data: { errorCode: 'MARKET_NOT_FOUND', message: 'Market with ID 999 not found', data: {} } } };
  const grpcErr = toGrpcError(openApiErr);
  assert.match(grpcErr.message, /^MARKET_NOT_FOUND: /);
});

test('the Send Txs Bot legacy error shape (statusCode, no code field) falls back to UNKNOWN', () => {
  const sendTxsBotErr = { response: { data: { statusCode: 400, message: 'Invalid signature' } } };
  const grpcErr = toGrpcError(sendTxsBotErr);
  assert.match(grpcErr.message, /^UNKNOWN: /);
});

test('unrecognized error shape falls back to UNKNOWN, not a guess', () => {
  const mysteryErr = new Error('something broke');
  const grpcErr = toGrpcError(mysteryErr);
  assert.match(grpcErr.message, /^UNKNOWN: /);
});

test('non-Error, non-object thrown value does not crash the mapper', () => {
  const grpcErr = toGrpcError('a bare string was thrown');
  assert.match(grpcErr.message, /^UNKNOWN: /);
});

test('a .code field that is not a real ApiErrorCodes member is not mistaken for one', () => {
  // e.g. a raw Node network error surfacing before it ever reaches the SDK,
  // ECONNREFUSED has the same shape as a Boros API error but isn't one
  const nodeErr = { code: 'ECONNREFUSED', message: 'connect ECONNREFUSED' };
  const grpcErr = toGrpcError(nodeErr);
  assert.match(grpcErr.message, /^UNKNOWN: /);
});
