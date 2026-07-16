// The rustic-on-R2 coordinates and resolved credentials carried by the
// managed /provision and /finalize requests. A prefix-scoped Cloudflare R2
// credential is an STS-style triple, so `session_token` must travel with its
// temporary access and secret keys.
export type WorkspaceRepo = {
  endpoint: string;
  region: string;
  bucket: string;
  prefix: string;
  access_key: string;
  secret_key: string;
  session_token?: string;
};

// Build the environment for the in-container workspace helper. Secrets stay
// out of argv and the §0 log; an absent session token preserves the long-lived
// credential path byte-for-byte.
export function workspaceExecEnv(repo: WorkspaceRepo, password: string): Record<string, string> {
  const env: Record<string, string> = {
    PATH: "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    HOME: "/root",
    PILLBOX_R2_ACCESS_KEY: repo.access_key,
    PILLBOX_R2_SECRET_KEY: repo.secret_key,
    PILLBOX_REPO_PASSWORD: password,
  };
  if (repo.session_token) env.PILLBOX_R2_SESSION_TOKEN = repo.session_token;
  return env;
}
