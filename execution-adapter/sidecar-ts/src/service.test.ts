import { test, mock } from 'node:test';
import assert from 'node:assert/strict';
import type * as grpc from '@grpc/grpc-js';
import type { Exchange, BorosBackend } from '@pendle/sdk-boros';
import { ExecutionServiceImpl } from './service';

// The real Exchange/BorosSendTxsBotSdk return types are huge (Exchange.placeOrder's
// result alone has a 90-variant events union), fakes here only cover what
// service.ts actually reads, `as any` on the fake construction is the
// standard pragmatic move for that, it doesn't loosen anything about the
// actual production code's types.

function makeExchange(placeOrderImpl?: (params: any) => any, cancelOrdersImpl?: (params: any) => any): Exchange {
  return {
    placeOrder: mock.fn(placeOrderImpl ?? (async (_p: any) => ({}))),
    cancelOrders: mock.fn(cancelOrdersImpl ?? (async (_p: any) => ({}))),
  } as any;
}

function makeStatusSdk(traceImpl?: (params: any) => any): BorosBackend.BorosSendTxsBotSdk {
  return { agent: { agentControllerTrace: mock.fn(traceImpl ?? (async (_p: any) => ({ data: {} }))) } } as any;
}

function makeCall(request: Record<string, unknown>): grpc.ServerUnaryCall<any, any> {
  return { request } as any;
}

function capturingCallback() {
  const calls: Array<[unknown, unknown]> = [];
  const callback = ((err: unknown, resp: unknown) => { calls.push([err, resp]); }) as grpc.sendUnaryData<any>;
  return { callback, calls };
}

test('placeOrder maps request fields and extracts filledSize.value, not String(filledSize)', async () => {
  // this is the regression test for the 2026-07-18 fix, filledSize used to
  // be String(FixedX18) which produces a decimal string rust-bridge can't
  // parse as i128, .value is the raw bigint that actually matches the wire
  // convention
  const exchange = makeExchange(async (_p: any) => ({
    executeResponse: { txHash: '0xabc', status: 'PROCESSED' },
    result: {
      order: {
        orderId: 123n,
        filledSize: { value: 1_500_000_000_000_000_000n }, // FixedX18-shaped fake
        placedSize: 2_000_000_000_000_000_000n,
      },
    },
  }));
  const service = ExecutionServiceImpl.createForTesting(exchange, makeStatusSdk());

  const call = makeCall({
    marketAcc: '0xdead',
    marketId: 7,
    side: 1, // proto Side.LONG (shifted +1 from the SDK's own enum)
    size: '1000000000000000000',
    tif: 1, // proto GOOD_TIL_CANCELLED
  });
  const { callback, calls } = capturingCallback();

  await service.placeOrder(call, callback);

  assert.equal(calls.length, 1);
  const [err, resp] = calls[0];
  assert.equal(err, null);
  assert.equal((resp as any).filledSize, '1500000000000000000', 'must be the raw bigint string, no decimal point');
  assert.equal((resp as any).placedSize, '2000000000000000000');
  assert.equal((resp as any).txHash, '0xabc');
  assert.equal((resp as any).orderId, '123');
  assert.equal((resp as any).status, 3); // PROCESSED

  const placeOrderMock = (exchange.placeOrder as any).mock;
  assert.equal(placeOrderMock.calls.length, 1);
  const sentArgs = placeOrderMock.calls[0].arguments[0];
  assert.equal(sentArgs.marketAcc, '0xdead');
  assert.equal(sentArgs.marketId, 7);
  assert.equal(sentArgs.size, 1_000_000_000_000_000_000n);
});

test('placeOrder with no order in the result defaults filledSize to 0, not a crash', async () => {
  const exchange = makeExchange(async (_p: any) => ({
    executeResponse: { txHash: '0xabc', status: 'PROCESSING' },
    result: {},
  }));
  const service = ExecutionServiceImpl.createForTesting(exchange, makeStatusSdk());

  const { callback, calls } = capturingCallback();
  await service.placeOrder(makeCall({ marketAcc: '0xdead', marketId: 1, side: 2, size: '5', tif: 2 }), callback);

  const [err, resp] = calls[0];
  assert.equal(err, null);
  assert.equal((resp as any).filledSize, '0');
  assert.equal((resp as any).orderId, undefined);
});

test('placeOrder rejection is mapped through toGrpcError, not thrown', async () => {
  const exchange = makeExchange(async (_p: any) => { throw { code: 'INSUFFICIENT_MARGIN', message: 'not enough margin' }; });
  const service = ExecutionServiceImpl.createForTesting(exchange, makeStatusSdk());

  const { callback, calls } = capturingCallback();
  await service.placeOrder(makeCall({ marketAcc: '0xdead', marketId: 1, side: 1, size: '5', tif: 1 }), callback);

  const [err, resp] = calls[0];
  assert.equal(resp, null);
  assert.match((err as grpc.ServiceError).message, /^INSUFFICIENT_MARGIN: /);
});

test('cancelOrders passes marketAcc/marketId/cancelAll/orderIds through and maps the response', async () => {
  const exchange = makeExchange(undefined, async (_p: any) => ({
    executeResponse: { txHash: '0xcancel', status: 'PROCESSED' },
  }));
  const service = ExecutionServiceImpl.createForTesting(exchange, makeStatusSdk());

  const { callback, calls } = capturingCallback();
  await service.cancelOrders(
    makeCall({ marketAcc: '0xdead', marketId: 3, cancelAll: false, orderIds: ['1', '2'] }),
    callback,
  );

  const [err, resp] = calls[0];
  assert.equal(err, null);
  assert.equal((resp as any).txHash, '0xcancel');
  assert.equal((resp as any).status, 3);

  const sentArgs = (exchange.cancelOrders as any).mock.calls[0].arguments[0];
  assert.deepEqual(sentArgs, { marketAcc: '0xdead', marketId: 3, cancelAll: false, orderIds: ['1', '2'] });
});

test('getTxStatus maps agentControllerTrace response fields, including both status enums', async () => {
  const statusSdk = makeStatusSdk(async (_p: any) => ({
    data: { submissionStatus: 'PROCESSED', nonceStatus: 'PROCESSING', txHash: '0xtrace' },
  }));
  const service = ExecutionServiceImpl.createForTesting(makeExchange(), statusSdk);

  const { callback, calls } = capturingCallback();
  await service.getTxStatus(makeCall({ agent: '0xagent', nonce: '5' }), callback);

  const [err, resp] = calls[0];
  assert.equal(err, null);
  assert.equal((resp as any).submissionStatus, 3); // PROCESSED
  assert.equal((resp as any).nonceStatus, 2); // PROCESSING
  assert.equal((resp as any).txHash, '0xtrace');

  const sentArgs = (statusSdk.agent.agentControllerTrace as any).mock.calls[0].arguments[0];
  assert.deepEqual(sentArgs, { agent: '0xagent', nonce: '5' });
});
