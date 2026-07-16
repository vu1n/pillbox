import assert from "node:assert/strict";
import test from "node:test";

import { workspaceExecEnv, type WorkspaceRepo } from "./src/workspace_repo.ts";

const repo: WorkspaceRepo = {
  endpoint: "https://account.r2.cloudflarestorage.com",
  region: "auto",
  bucket: "workspaces",
  prefix: "project/run/",
  access_key: "scoped-ak",
  secret_key: "scoped-sk",
};

test("scoped R2 credentials forward their session token to the workspace helper", () => {
  const env = workspaceExecEnv({ ...repo, session_token: "scoped-session-token" }, "repo-password");
  assert.equal(env.PILLBOX_R2_SESSION_TOKEN, "scoped-session-token");
});

test("long-lived R2 credentials omit the session-token environment variable", () => {
  const env = workspaceExecEnv(repo, "repo-password");
  assert.equal("PILLBOX_R2_SESSION_TOKEN" in env, false);
});
