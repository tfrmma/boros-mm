import { test } from 'node:test';
import assert from 'node:assert/strict';
import { EnvSecretProvider, AwsSecretsManagerProvider, secretProviderFromEnv } from './secrets';

test('EnvSecretProvider throws when BOROS_AGENT_PRIVATE_KEY is missing', async () => {
  delete process.env.BOROS_AGENT_PRIVATE_KEY;
  await assert.rejects(() => new EnvSecretProvider().getAgentPrivateKey(), /missing required env var/);
});

test('EnvSecretProvider returns the raw env var value', async () => {
  process.env.BOROS_AGENT_PRIVATE_KEY = '0xabc123';
  assert.equal(await new EnvSecretProvider().getAgentPrivateKey(), '0xabc123');
  delete process.env.BOROS_AGENT_PRIVATE_KEY;
});

function mockSecretsClient(secretString: string | undefined) {
  return { send: async () => ({ SecretString: secretString }) };
}

test('AwsSecretsManagerProvider adds the 0x prefix if the stored secret is missing it', async () => {
  const provider = new AwsSecretsManagerProvider('test/secret', mockSecretsClient('abc123'));
  assert.equal(await provider.getAgentPrivateKey(), '0xabc123');
});

test('AwsSecretsManagerProvider does not double-prefix a secret that already has 0x', async () => {
  const provider = new AwsSecretsManagerProvider('test/secret', mockSecretsClient('0xabc123'));
  assert.equal(await provider.getAgentPrivateKey(), '0xabc123');
});

test('AwsSecretsManagerProvider trims surrounding whitespace before checking the prefix', async () => {
  const provider = new AwsSecretsManagerProvider('test/secret', mockSecretsClient('  0xabc123\n'));
  assert.equal(await provider.getAgentPrivateKey(), '0xabc123');
});

test('AwsSecretsManagerProvider throws on a binary secret (no SecretString)', async () => {
  const provider = new AwsSecretsManagerProvider('test/secret', mockSecretsClient(undefined));
  await assert.rejects(() => provider.getAgentPrivateKey(), /has no SecretString/);
});

test('secretProviderFromEnv defaults to EnvSecretProvider', () => {
  delete process.env.SECRETS_PROVIDER;
  assert.ok(secretProviderFromEnv() instanceof EnvSecretProvider);
});

test('secretProviderFromEnv builds AwsSecretsManagerProvider and requires AWS_SECRET_ID', () => {
  process.env.SECRETS_PROVIDER = 'aws-secrets-manager';
  delete process.env.AWS_SECRET_ID;
  assert.throws(() => secretProviderFromEnv(), /requires AWS_SECRET_ID/);

  process.env.AWS_SECRET_ID = 'boros/agent-key';
  assert.ok(secretProviderFromEnv() instanceof AwsSecretsManagerProvider);

  delete process.env.SECRETS_PROVIDER;
  delete process.env.AWS_SECRET_ID;
});

test('secretProviderFromEnv rejects an unknown provider name', () => {
  process.env.SECRETS_PROVIDER = 'vault';
  assert.throws(() => secretProviderFromEnv(), /unknown SECRETS_PROVIDER/);
  delete process.env.SECRETS_PROVIDER;
});
