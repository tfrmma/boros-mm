import { SecretsManagerClient, GetSecretValueCommand } from '@aws-sdk/client-secrets-manager';
import { Hex } from 'viem';

/**
 * Where the agent private key comes from. `env` is local-dev-only, see
 * `EnvSecretProvider`'s own doc comment. `aws-secrets-manager` is the one
 * real production path implemented here. Adding another (Vault, GCP
 * Secret Manager) means adding another `SecretProvider` and a case in
 * `secretProviderFromEnv`, the rest of this process doesn't change.
 */
export interface SecretProvider {
  getAgentPrivateKey(): Promise<Hex>;
}

/**
 * Local development only. Storing a signing key in a plain env var is
 * fine on a laptop, not fine on anything that logs its environment or
 * gets inspected by another process. `secretProviderFromEnv` defaults to
 * this so the sidecar still runs out of the box for local dev, but
 * production deployments should set `SECRETS_PROVIDER=aws-secrets-manager`
 * (or add a provider for whatever secrets store is actually in use).
 */
export class EnvSecretProvider implements SecretProvider {
  async getAgentPrivateKey(): Promise<Hex> {
    const v = process.env.BOROS_AGENT_PRIVATE_KEY;
    if (!v) throw new Error('missing required env var: BOROS_AGENT_PRIVATE_KEY');
    return v as Hex;
  }
}

/**
 * Reads the agent private key from a real AWS Secrets Manager secret. The
 * secret's `SecretString` is expected to be the raw hex private key
 * (with or without the `0x` prefix), not a JSON blob, this process only
 * ever needs the one value, no reason to add a parsing layer for it.
 * Binary secrets (`SecretBinary`) aren't supported, Secrets Manager only
 * returns one or the other, and a signing key has no reason to be binary.
 *
 * AWS credentials come from the SDK's own default provider chain (env
 * vars, an attached IAM role, `~/.aws/config`, ...), not handled here,
 * that's the SDK's job and it already does it correctly.
 */
export class AwsSecretsManagerProvider implements SecretProvider {
  constructor(private readonly secretId: string, private readonly client: Pick<SecretsManagerClient, 'send'> = new SecretsManagerClient({})) {}

  async getAgentPrivateKey(): Promise<Hex> {
    const result = await this.client.send(new GetSecretValueCommand({ SecretId: this.secretId }));
    if (!result.SecretString) {
      throw new Error(`AWS Secrets Manager secret "${this.secretId}" has no SecretString (binary secrets aren't supported)`);
    }
    const trimmed = result.SecretString.trim();
    return (trimmed.startsWith('0x') ? trimmed : `0x${trimmed}`) as Hex;
  }
}

/** Picks and configures a `SecretProvider` from `SECRETS_PROVIDER` (defaults to `env`). */
export function secretProviderFromEnv(): SecretProvider {
  const kind = process.env.SECRETS_PROVIDER ?? 'env';
  switch (kind) {
    case 'env':
      return new EnvSecretProvider();
    case 'aws-secrets-manager': {
      const secretId = process.env.AWS_SECRET_ID;
      if (!secretId) throw new Error('SECRETS_PROVIDER=aws-secrets-manager requires AWS_SECRET_ID to be set');
      const region = process.env.AWS_REGION;
      return new AwsSecretsManagerProvider(secretId, new SecretsManagerClient(region ? { region } : {}));
    }
    default:
      throw new Error(`unknown SECRETS_PROVIDER: "${kind}" (expected "env" or "aws-secrets-manager")`);
  }
}
