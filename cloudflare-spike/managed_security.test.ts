import assert from "node:assert/strict";
import { test } from "node:test";
import { decryptOAuthMaterial, encryptOAuthMaterial } from "./src/credentials/crypto.ts";
import { makeEvidenceReadCurrentnessRequest, validateEvidenceBinding, validateEvidenceReadClaims, validatePositionalEvidenceSelector, validateSessionRef } from "./src/managed_contract.ts";

test("OAuth material is versioned, encrypted, and authenticated with binding AAD", async () => {
  const material = { access_token: "access-secret", refresh_token: "refresh-secret", expires_at: 200, provider_host: "api.example.test", generation: 1 };
  const ciphertext = await encryptOAuthMaterial("deployment-key", material, "pillbox-credential/v1:binding-1");
  assert.equal(ciphertext.includes("access-secret"), false);
  assert.deepEqual(await decryptOAuthMaterial("deployment-key", ciphertext, "pillbox-credential/v1:binding-1"), material);
  await assert.rejects(() => decryptOAuthMaterial("deployment-key", ciphertext, "pillbox-credential/v1:binding-2"));
});

test("evidence reads require a positional or snapshot-bound reference and containment", () => {
  const claims = validateEvidenceReadClaims({
    version: "huddles.evidence-read-grant/1",
    grant_id: "evidence-1",
    installation: { installation_id: "install-1", execution_realm_id: "realm-1", protocol_revision: "pillbox.huddles/1" },
    workspace_id: "ws-1",
    viewer_principal_id: "viewer-1",
    policy_id: "policy-1",
    run_id: "run-1",
    session_ref: { realm: { runtime: "pillbox", execution_realm_id: "realm-1" }, session_id: "session-1", seq_range: [10, 20] },
    issued_at: 100,
    not_before: 100,
    expires_at: 120,
  });
  const expected = { ...claims, session_ref: { ...claims.session_ref, seq: 15, seq_range: undefined } };
  assert.equal(validateEvidenceBinding(claims, expected), undefined);
  assert.equal(validateEvidenceBinding(claims, { ...expected, session_ref: { ...expected.session_ref, seq: 25 } }), "session_mismatch");
  assert.equal(validateEvidenceBinding(claims, { ...expected, session_ref: { ...expected.session_ref, seq: undefined, seq_range: undefined } }), "session_mismatch");
  assert.throws(() => validateSessionRef({ ...claims.session_ref, seq: 15, seq_range: [10, 20] }), /both seq and seq_range/);
  assert.throws(() => validatePositionalEvidenceSelector({ ...claims.session_ref, seq_range: undefined, event_cursor: "cursor-1" }), /positional selector/);
  assert.throws(() => validatePositionalEvidenceSelector({ ...claims.session_ref, seq: undefined, seq_range: undefined }), /positional selector/);
  assert.throws(() => validatePositionalEvidenceSelector({ ...claims.session_ref, seq: 15, seq_range: undefined, snapshot_ref: "snapshot-1" }), /cursor or snapshot selectors/);
  const currentness = makeEvidenceReadCurrentnessRequest(claims, {
    installation: claims.installation,
    workspace_id: claims.workspace_id,
    viewer_principal_id: claims.viewer_principal_id,
    policy_id: claims.policy_id,
    run_id: claims.run_id,
    session_ref: claims.session_ref,
  }, { algorithm: "Ed25519", key_id: "key-1", public_key_sha256: "sha256:" + "a".repeat(64) });
  assert.equal(currentness.version, "pillbox.authorization-currentness/2");
  assert.equal(currentness.verified_signer.public_key_sha256, "sha256:" + "a".repeat(64));
});
