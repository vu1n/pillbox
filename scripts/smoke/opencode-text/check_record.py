"""Check a live-turn record without printing credentials or native evidence."""
import json
from pathlib import Path
import sys

record = json.loads(Path(sys.argv[1]).read_text())["execution"]
assert record["status"] == "completed", record["status"]
detail = record["detail"]
assert detail["output_text"].strip() == "PILLBOX_OPENCODE_TEXT_OK"
assert 0 < len(detail["output_text"].encode()) <= 32768
resolved = detail["resolved"]
assert set(resolved) == {"harness", "harness_version", "adapter_revision", "runner_image_id", "requested_model", "served_model"}
assert resolved["harness"] == "opencode"
assert resolved["harness_version"].startswith("2.")
assert resolved["adapter_revision"] == "pillbox/opencode-text-v2/1"
assert resolved["runner_image_id"].startswith("sha256:")
assert resolved["served_model"] is None
assert detail["session_ref"]
usage = detail.get("usage")
if usage is not None:
    assert "cost_usd" in usage
    assert set(usage) <= {"cost_usd", "input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens"}
    for name, value in usage.items():
        if name != "cost_usd": assert isinstance(value, int) and 0 <= value <= 10_000_000_000
print("completed: bounded final text, observed resolved metadata and closed usage shape")
