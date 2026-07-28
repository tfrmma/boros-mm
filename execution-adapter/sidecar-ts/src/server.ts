import * as grpc from '@grpc/grpc-js';
import * as protoLoader from '@grpc/proto-loader';
import * as path from 'path';
import { Hex } from 'viem';

import { loadConfig } from './config';
import { ExecutionServiceImpl } from './service';

function requiredEnv(name: string): string {
  const v = process.env[name];
  if (!v) throw new Error(`missing required env var: ${name}`);
  return v;
}

async function main() {
  const config = loadConfig();

  // agent private key: local-dev-only env var. See ExecutionServiceImpl.create's
  // doc comment, production deployments must source this from a proper
  // secrets manager, and the root wallet's key must never be a parameter
  // to this process at all.
  const agentPrivateKey = requiredEnv('BOROS_AGENT_PRIVATE_KEY') as Hex;
  const apiBaseUrl = requiredEnv('BOROS_API_BASE_URL');

  const packageDef = protoLoader.loadSync(path.resolve(__dirname, '../execution.proto'), {
    keepCase: true,
    longs: String,
    enums: Number,
    defaults: true,
    oneofs: true,
  });
  const proto = grpc.loadPackageDefinition(packageDef) as any;
  const serviceDef = proto.boros.execution.v1.ExecutionService.service;

  const impl = await ExecutionServiceImpl.create(config, agentPrivateKey, apiBaseUrl);

  const server = new grpc.Server();
  server.addService(serviceDef, {
    placeOrder: impl.placeOrder.bind(impl),
    cancelOrders: impl.cancelOrders.bind(impl),
    getTxStatus: impl.getTxStatus.bind(impl),
  });

  server.bindAsync(config.listenAddr, grpc.ServerCredentials.createInsecure(), (err, port) => {
    if (err) {
      console.error('failed to bind:', err);
      process.exit(1);
    }
    console.log(`sidecar-ts listening on ${config.listenAddr} (port ${port})`);
  });
}

main().catch((err) => {
  console.error('sidecar-ts fatal startup error:', err);
  process.exit(1);
});
