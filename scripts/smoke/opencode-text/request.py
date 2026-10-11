"""Prepare one harness-agnostic live-turn request; never runs inference."""
import argparse
import hashlib
import json
from pathlib import Path
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--model", required=True, help="native provider/modelID from the selected image's catalog")
parser.add_argument("--effort", required=True, help="exact variant from that model's image catalog")
parser.add_argument("--output", required=True)
args = parser.parse_args()
invocation = "opencode_text_" + uuid.uuid4().hex
text = "Reply with exactly PILLBOX_OPENCODE_TEXT_OK. Do not use tools."
request = {"contract_version": "pillbox.text/2", "session_ref": {"session_id": invocation},
           "invocation_id": invocation, "idempotency_key": invocation, "rendered_input": text,
           "rendered_input_hash": "sha256:" + hashlib.sha256(text.encode()).hexdigest(),
           "tool_policy": "deny_all", "agent": {"harness": "opencode", "model": args.model,
           "reasoning_effort": args.effort}, "placement": "local_microvm",
           "output_format": {"kind": "text", "retry_count": 0}, "limits": {"timeout_ms": 120000,
           "max_final_text_bytes": 32768, "max_frame_bytes": 1048576, "max_evidence_bytes": 8388608}}
Path(args.output).write_text(json.dumps(request, indent=2) + "\n")
print(invocation)
