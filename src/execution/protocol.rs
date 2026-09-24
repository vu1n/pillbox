//! I/O-free versioned Codex profiles for the host-owned repository broker.
//!
//! The adjacent source catalogs preserve exact model objects from OpenAI Codex
//! tags `rust-v0.151.0` (commit `78c290807ce710180111df227df3b7a4fe845452`)
//! and `rust-v0.156.1` (commit `b412ff32c417f855c2b2d1581b77058eed87c84b`).
//! The legacy effective catalog changes
//! only `tool_mode` to `direct`. The GPT-6 effective catalog also clears model-
//! advertised utility tools, which otherwise bypass the broker-only surface.
//! Code-mode exec/wait bypass pre-tool hooks, so only direct broker functions
//! can satisfy a host-enforced budget for every tool.
//! Model, provider, effort, and Responses Lite routing remain unchanged. Live
//! provider acceptance is an integration gate; rejection must never fall back.
//! Source `core/src/tools/spec_plan.rs` gates native environment handlers on an
//! environment selection. The caller must launch with a fresh home, an empty
//! working directory, and no repository share;
//! this module generates configuration, not an OS sandbox or credential store.

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::Deserialize;
use serde_json::{json, Value};

use super::files::{FileOperation, MAX_FILE_BYTES, MAX_PATH_BYTES};

pub(crate) const CODEX_VERSION: &str = "0.151.0";
pub(crate) const GPT6_CODEX_VERSION: &str = "0.156.1";
pub(crate) const PROVIDER_ID: &str = "pillbox_openai_http";
pub(crate) const CODEX_CWD: &str = "/workspace";
pub(crate) const PINNED_MODEL_CATALOG: &str = include_str!("codex-models-0.151.0.json");
pub(crate) const GPT6_MODEL_CATALOG: &str = include_str!("codex-models-0.156.1-gpt6.json");
pub(crate) const SOURCE_CATALOG_SHA256: &str =
    "sha256:fba33ea2414335b8bb3ba6741d17e72ef6e2b6f54a86470907b54451e5b772af";
pub(crate) const EFFECTIVE_CATALOG_SHA256: &str =
    "sha256:aa0ff087c5f495c6cf26cf979369b6d6b9080cd39239090444e8d1b968f56b8a";
pub(crate) const GPT6_SOURCE_CATALOG_SHA256: &str =
    "sha256:4523084c4760faf6b4c20fe87bd4e94dd5315d8d1b1b63589dcc214fc152cebe";
pub(crate) const GPT6_EFFECTIVE_CATALOG_SHA256: &str =
    "sha256:f475bd3a7b94420fb9cbd4cc6214cf31e91f1e10bc6ed3a88fb7c73e772ed8e0";
pub(crate) const MAX_RENDERED_INPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_ID_BYTES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CodexProfile {
    version: &'static str,
    model: String,
    effort: String,
}

impl CodexProfile {
    #[cfg(test)]
    pub(crate) fn new(model: &str, effort: &str) -> Result<Self> {
        Self::new_for_version(CODEX_VERSION, model, effort)
    }

    pub(crate) fn new_for_version(version: &str, model: &str, effort: &str) -> Result<Self> {
        let (version, catalog) = match version {
            CODEX_VERSION => {
                ensure!(
                    matches!(model, "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna"),
                    "unsupported Codex model for version"
                );
                (CODEX_VERSION, PINNED_MODEL_CATALOG)
            }
            GPT6_CODEX_VERSION => {
                ensure!(
                    matches!(model, "gpt-6-sol" | "gpt-6-luna"),
                    "unsupported Codex model for version"
                );
                (GPT6_CODEX_VERSION, GPT6_MODEL_CATALOG)
            }
            _ => bail!("unsupported Codex version"),
        };
        let catalog: Value =
            serde_json::from_str(catalog).context("invalid embedded Codex model catalog")?;
        let metadata = catalog["models"]
            .as_array()
            .and_then(|models| models.iter().find(|entry| entry["slug"] == model))
            .context("model missing from embedded Codex catalog")?;
        ensure!(
            metadata["supported_reasoning_levels"]
                .as_array()
                .is_some_and(|levels| levels.iter().any(|level| level["effort"] == effort)),
            "unsupported reasoning effort for requested Codex model"
        );
        Ok(Self {
            version,
            model: model.to_owned(),
            effort: effort.to_owned(),
        })
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn version(&self) -> &str {
        self.version
    }

    pub(crate) fn source_catalog_digest(&self) -> &str {
        match self.version {
            CODEX_VERSION => SOURCE_CATALOG_SHA256,
            GPT6_CODEX_VERSION => GPT6_SOURCE_CATALOG_SHA256,
            _ => unreachable!("profile version is constructor validated"),
        }
    }

    pub(crate) fn effective_catalog_digest(&self) -> &str {
        match self.version {
            CODEX_VERSION => EFFECTIVE_CATALOG_SHA256,
            GPT6_CODEX_VERSION => GPT6_EFFECTIVE_CATALOG_SHA256,
            _ => unreachable!("profile version is constructor validated"),
        }
    }

    pub(crate) fn effort(&self) -> &str {
        &self.effort
    }

    /// Generate the entire fresh config; no caller-supplied config is merged.
    pub(crate) fn config_toml(&self, catalog_path: &str) -> Result<String> {
        ensure!(
            catalog_path.starts_with('/')
                && catalog_path.len() <= 1024
                && catalog_path
                    .split('/')
                    .skip(1)
                    .all(|part| !part.is_empty() && part != "." && part != "..")
                && !catalog_path.chars().any(char::is_control),
            "catalog path must be a bounded absolute guest file path"
        );
        // A separate key is required: Codex ignores overrides of built-in `openai`.
        // The OpenAI name and first-party auth flag preserve provider semantics.
        let config = json!({
            "model": self.model,
            "model_provider": PROVIDER_ID,
            "model_reasoning_effort": self.effort,
            "model_catalog_json": catalog_path,
            "approval_policy": "never",
            "approvals_reviewer": "user",
            "sandbox_mode": "read-only",
            "web_search": "disabled",
            "project_doc_max_bytes": 0,
            "project_doc_fallback_filenames": [],
            "cli_auth_credentials_store": "file",
            "mcp_oauth_credentials_store": "file",
            "mcp_servers": {},
            "notify": [],
            "model_providers": {
                PROVIDER_ID: {
                    "name": "OpenAI",
                    "base_url": "https://chatgpt.com/backend-api/codex",
                    "wire_api": "responses",
                    "requires_openai_auth": true,
                    "supports_websockets": false,
                    "supports_standalone_web_search": false,
                    "http_headers": { "version": self.version }
                }
            },
            "agents": { "enabled": false },
            "tools": {
                "update_plan": { "enabled": false },
                "experimental_request_user_input": { "enabled": false }
            },
            "orchestrator": {
                "skills": { "enabled": false },
                "mcp": { "enabled": false }
            },
            "skills": { "bundled": { "enabled": false }, "include_instructions": false },
            "features": {
                "shell_tool": false,
                "multi_agent": false,
                "multi_agent_v2": false,
                "apps": false,
                "plugins": false,
                "tool_suggest": false,
                "image_generation": false,
                "memories": false,
                "goals": false,
                "token_budget": false,
                "current_time_reminder": false,
                "deferred_executor": false,
                "hooks": false,
                "code_mode": false,
                "code_mode_host": false,
                "code_mode_only": false
            }
        });
        toml::to_string(&config).context("serialize pinned Codex config")
    }

    pub(crate) fn thread_start(&self, operations: &[FileOperation]) -> Result<Value> {
        Ok(json!({
            "model": self.model,
            "modelProvider": PROVIDER_ID,
            "allowProviderModelFallback": false,
            "approvalPolicy": "never",
            "approvalsReviewer": "user",
            "sandbox": "read-only",
            "cwd": CODEX_CWD,
            "ephemeral": true,
            "environments": [],
            "runtimeWorkspaceRoots": [],
            "selectedCapabilityRoots": [],
            "dynamicTools": dynamic_tools(operations)?,
            "experimentalRawEvents": false
        }))
    }

    pub(crate) fn turn_start(&self, thread_id: &str, rendered_input: &str) -> Result<Value> {
        validate_id(thread_id)?;
        ensure!(
            !rendered_input.is_empty() && rendered_input.len() <= MAX_RENDERED_INPUT_BYTES,
            "rendered input must contain 1..={MAX_RENDERED_INPUT_BYTES} bytes"
        );
        // Omission retains the thread's explicit empty environment selection.
        Ok(json!({
            "threadId": thread_id,
            "model": self.model,
            "effort": self.effort,
            "input": [{ "type": "text", "text": rendered_input, "text_elements": [] }]
        }))
    }

    /// Validate the result payload after the caller correlates the JSON-RPC id.
    /// 0.151 does not echo environments; empty roots and instruction sources are
    /// additional checks, not a replacement for the explicit request and config.
    pub(crate) fn validate_thread_start(&self, response: &Value) -> Result<String> {
        ensure!(
            response["model"] == self.model,
            "Codex changed requested model"
        );
        ensure!(
            response["modelProvider"] == PROVIDER_ID,
            "Codex changed provider"
        );
        ensure!(
            response["reasoningEffort"] == self.effort,
            "Codex changed reasoning effort"
        );
        ensure!(
            response["approvalPolicy"] == "never",
            "Codex changed approval policy"
        );
        ensure!(
            response["approvalsReviewer"] == "user",
            "Codex changed approval reviewer"
        );
        ensure!(
            response["cwd"] == CODEX_CWD,
            "Codex changed working directory"
        );
        ensure!(
            response["runtimeWorkspaceRoots"] == json!([]),
            "Codex enabled workspace roots"
        );
        ensure!(
            response["instructionSources"] == json!([]),
            "Codex loaded ambient instructions"
        );
        let sandbox = response.get("sandbox").context("missing Codex sandbox")?;
        ensure!(
            sandbox["type"] == "readOnly",
            "Codex changed sandbox policy"
        );
        ensure!(
            sandbox
                .get("networkAccess")
                .is_none_or(|value| value == false),
            "Codex enabled sandbox network access"
        );
        let thread = response.get("thread").context("missing Codex thread")?;
        ensure!(
            thread["cliVersion"] == self.version,
            "unsupported Codex version"
        );
        ensure!(
            thread["modelProvider"] == PROVIDER_ID,
            "thread provider mismatch"
        );
        ensure!(
            thread["cwd"] == CODEX_CWD,
            "thread working directory mismatch"
        );
        ensure!(
            thread["ephemeral"] == true,
            "Codex persisted the execution thread"
        );
        let id = thread["id"].as_str().context("missing Codex thread id")?;
        validate_id(id)?;
        Ok(id.to_owned())
    }
}

/// Preserve each source artifact and apply only the version's reviewed tool
/// presentation overrides. The caller writes these bytes at `config_toml`'s path.
impl CodexProfile {
    pub(crate) fn effective_catalog_json(&self) -> Result<String> {
        let source = match self.version {
            CODEX_VERSION => PINNED_MODEL_CATALOG,
            GPT6_CODEX_VERSION => GPT6_MODEL_CATALOG,
            _ => unreachable!("profile version is constructor validated"),
        };
        let mut catalog: Value =
            serde_json::from_str(source).context("invalid embedded Codex model catalog")?;
        for model in catalog["models"]
            .as_array_mut()
            .context("missing embedded models")?
        {
            model["tool_mode"] = json!("direct");
            if self.version == GPT6_CODEX_VERSION {
                model["experimental_supported_tools"] = json!([]);
            }
        }
        serde_json::to_string(&catalog).context("serialize effective Codex model catalog")
    }
}

pub(crate) fn initialize_params() -> Value {
    json!({
        "clientInfo": { "name": "pillbox_repository_execution", "version": env!("CARGO_PKG_VERSION") },
        "capabilities": { "experimentalApi": true, "requestAttestation": false }
    })
}

pub(crate) fn dynamic_tools(operations: &[FileOperation]) -> Result<Vec<Value>> {
    validate_operations(operations)?;
    Ok(operations.iter().map(|operation| {
        let (name, description, properties, required) = match operation {
            FileOperation::Read => ("pillbox_read_file", "Read one exact authorized repository file. Returns path, executable mode, encoding and content; UTF-8 when valid, otherwise base64.", json!({"path": path_schema()}), json!(["path"])),
            FileOperation::Remove => ("pillbox_remove_file", "Remove one exact authorized repository file.", json!({"path": path_schema()}), json!(["path"])),
            FileOperation::Write => ("pillbox_write_file", "Replace one exact authorized repository file with content and executable mode. Use utf8 for text, base64 for binary. Does not disclose previous contents.", json!({
                "path": path_schema(),
                "encoding": {"type": "string", "enum": ["utf8", "base64"]},
                "content": {"type": "string", "maxLength": MAX_FILE_BYTES.div_ceil(3) * 4},
                "executable": {"type": "boolean"}
            }), json!(["path", "executable", "encoding", "content"])),
        };
        json!({
            "type": "function", "name": name, "description": description,
            "deferLoading": false,
            "inputSchema": {"type": "object", "properties": properties, "required": required, "additionalProperties": false}
        })
    }).collect())
}

fn path_schema() -> Value {
    json!({ "type": "string", "minLength": 1, "maxLength": MAX_PATH_BYTES })
}

fn validate_operations(operations: &[FileOperation]) -> Result<()> {
    ensure!(
        operations.len() <= 3 && operations.windows(2).all(|pair| pair[0] < pair[1]),
        "file operations must be sorted and unique"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FileCall {
    Read {
        path: String,
    },
    Remove {
        path: String,
    },
    Write {
        path: String,
        executable: bool,
        bytes: Vec<u8>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DynamicCall {
    pub(crate) call_id: String,
    pub(crate) operation: FileCall,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DynamicCallParams {
    thread_id: String,
    turn_id: String,
    call_id: String,
    namespace: Option<String>,
    tool: String,
    arguments: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum FileEncoding {
    Utf8,
    Base64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    encoding: FileEncoding,
    content: String,
    executable: bool,
}

/// Parse one native item/tool/call payload. The broker separately enforces exact
/// path grants, operation grants and cumulative limits before reading or writing.
pub(crate) fn parse_dynamic_call(
    params: &Value,
    thread_id: &str,
    turn_id: &str,
    operations: &[FileOperation],
    max_write_bytes: u64,
) -> Result<DynamicCall> {
    validate_id(thread_id)?;
    validate_id(turn_id)?;
    validate_operations(operations)?;
    ensure!(
        max_write_bytes > 0 && max_write_bytes <= MAX_FILE_BYTES,
        "invalid write byte limit"
    );
    let params: DynamicCallParams =
        serde_json::from_value(params.clone()).context("invalid dynamic tool call")?;
    ensure!(
        params.thread_id == thread_id && params.turn_id == turn_id,
        "uncorrelated dynamic tool call"
    );
    validate_id(&params.call_id)?;
    ensure!(
        params.namespace.is_none(),
        "unsupported dynamic tool namespace"
    );
    let requested = match params.tool.as_str() {
        "pillbox_read_file" => FileOperation::Read,
        "pillbox_remove_file" => FileOperation::Remove,
        "pillbox_write_file" => FileOperation::Write,
        _ => bail!("unsupported dynamic tool"),
    };
    ensure!(
        operations.contains(&requested),
        "dynamic tool operation not admitted"
    );
    let operation = match requested {
        FileOperation::Read | FileOperation::Remove => {
            let args: PathArgs =
                serde_json::from_value(params.arguments).context("invalid file tool arguments")?;
            validate_path_size(&args.path)?;
            if requested == FileOperation::Read {
                FileCall::Read { path: args.path }
            } else {
                FileCall::Remove { path: args.path }
            }
        }
        FileOperation::Write => {
            let args: WriteArgs =
                serde_json::from_value(params.arguments).context("invalid write tool arguments")?;
            validate_path_size(&args.path)?;
            let bytes = match args.encoding {
                FileEncoding::Utf8 => {
                    ensure!(
                        args.content.len() as u64 <= max_write_bytes,
                        "UTF-8 write exceeds file byte limit"
                    );
                    args.content.into_bytes()
                }
                FileEncoding::Base64 => {
                    ensure!(
                        args.content.len() as u64 <= max_write_bytes.div_ceil(3) * 4,
                        "encoded write exceeds file byte limit"
                    );
                    ensure!(
                        args.content.len().is_multiple_of(4),
                        "invalid base64 file contents"
                    );
                    let padding = args
                        .content
                        .bytes()
                        .rev()
                        .take_while(|byte| *byte == b'=')
                        .count();
                    ensure!(padding <= 2, "invalid base64 file contents");
                    let decoded_len = args.content.len() / 4 * 3 - padding;
                    ensure!(
                        decoded_len as u64 <= max_write_bytes,
                        "decoded write exceeds file byte limit"
                    );
                    let mut bytes = vec![0; decoded_len];
                    let written = BASE64
                        .decode_slice(args.content, &mut bytes)
                        .context("invalid base64 file contents")?;
                    ensure!(written == decoded_len, "invalid base64 decoded length");
                    bytes
                }
            };
            FileCall::Write {
                path: args.path,
                executable: args.executable,
                bytes,
            }
        }
    };
    Ok(DynamicCall {
        call_id: params.call_id,
        operation,
    })
}

fn validate_path_size(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= MAX_PATH_BYTES,
        "file path is empty or too long"
    );
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= MAX_ID_BYTES
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
        "invalid native correlation id"
    );
    Ok(())
}

pub(crate) fn validate_turn_started(response: &Value) -> Result<String> {
    let turn = response.get("turn").context("missing native turn")?;
    ensure!(
        turn["status"] == "inProgress",
        "new native turn is not in progress"
    );
    ensure!(turn["error"].is_null(), "new native turn contains an error");
    ensure!(
        turn["items"].is_array(),
        "native turn items must be an array"
    );
    let id = turn["id"].as_str().context("missing native turn id")?;
    validate_id(id)?;
    Ok(id.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnTerminal {
    Completed,
    Failed,
    Interrupted,
}

pub(crate) fn validate_turn_terminal(
    params: &Value,
    thread_id: &str,
    turn_id: &str,
) -> Result<TurnTerminal> {
    validate_id(thread_id)?;
    validate_id(turn_id)?;
    ensure!(
        params["threadId"] == thread_id,
        "uncorrelated terminal thread"
    );
    let turn = params.get("turn").context("missing terminal turn")?;
    ensure!(turn["id"] == turn_id, "uncorrelated terminal turn");
    ensure!(
        turn["items"].is_array(),
        "terminal turn items must be an array"
    );
    match turn["status"].as_str() {
        Some("completed") if turn["error"].is_null() => Ok(TurnTerminal::Completed),
        Some("interrupted") if turn["error"].is_null() => Ok(TurnTerminal::Interrupted),
        Some("failed")
            if turn["error"]["message"]
                .as_str()
                .is_some_and(|s| !s.is_empty()) =>
        {
            Ok(TurnTerminal::Failed)
        }
        _ => bail!("invalid native terminal status or error"),
    }
}

/// Deny every unsupported server request without reflecting untrusted content.
pub(crate) fn unsupported_server_request(id: &Value) -> Result<Value> {
    match id {
        Value::String(id) => validate_id(id)?,
        Value::Number(id) if id.as_i64().is_some() => {}
        _ => bail!("invalid native request id"),
    }
    Ok(
        json!({"id": id, "error": {"code": -32601, "message": "Request unsupported by the bounded repository execution profile"}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn profile() -> CodexProfile {
        CodexProfile::new("gpt-5.6-sol", "high").unwrap()
    }

    fn thread_response() -> Value {
        json!({
            "model": "gpt-5.6-sol", "modelProvider": PROVIDER_ID,
            "reasoningEffort": "high", "approvalPolicy": "never",
            "approvalsReviewer": "user", "cwd": CODEX_CWD,
            "runtimeWorkspaceRoots": [], "instructionSources": [],
            "sandbox": {"type": "readOnly", "networkAccess": false},
            "thread": {"id": "thread-1", "cliVersion": CODEX_VERSION,
                "modelProvider": PROVIDER_ID, "cwd": CODEX_CWD, "ephemeral": true}
        })
    }

    fn call(tool: &str, arguments: Value) -> Value {
        json!({"threadId": "thread-1", "turnId": "turn-1", "callId": "call-1",
            "tool": tool, "namespace": null, "arguments": arguments})
    }

    #[test]
    fn catalog_matches_unchanged_pinned_source_and_closed_model_set() {
        assert_eq!(
            format!(
                "sha256:{:x}",
                Sha256::digest(PINNED_MODEL_CATALOG.as_bytes())
            ),
            SOURCE_CATALOG_SHA256
        );
        let catalog: Value = serde_json::from_str(PINNED_MODEL_CATALOG).unwrap();
        let models = catalog["models"].as_array().unwrap();
        assert_eq!(models.len(), 3);
        for (model, slug) in models
            .iter()
            .zip(["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"])
        {
            assert_eq!(model["slug"], slug);
            assert_eq!(model["tool_mode"], "code_mode_only");
            assert_eq!(model["use_responses_lite"], true);
            assert_eq!(model["experimental_supported_tools"], json!([]));
        }
    }

    #[test]
    fn effective_catalog_changes_only_tool_presentation() {
        assert_eq!(
            format!(
                "sha256:{:x}",
                Sha256::digest(profile().effective_catalog_json().unwrap().as_bytes())
            ),
            EFFECTIVE_CATALOG_SHA256
        );
        let source: Value = serde_json::from_str(PINNED_MODEL_CATALOG).unwrap();
        let mut effective: Value =
            serde_json::from_str(&profile().effective_catalog_json().unwrap()).unwrap();
        for model in effective["models"].as_array_mut().unwrap() {
            assert_eq!(model["tool_mode"], "direct");
            model["tool_mode"] = json!("code_mode_only");
        }
        assert_eq!(effective, source);
    }

    #[test]
    fn gpt6_catalog_preserves_source_and_closes_advertised_utility_tools() {
        let profile =
            CodexProfile::new_for_version(GPT6_CODEX_VERSION, "gpt-6-sol", "high").unwrap();
        assert_eq!(
            format!("sha256:{:x}", Sha256::digest(GPT6_MODEL_CATALOG.as_bytes())),
            GPT6_SOURCE_CATALOG_SHA256
        );
        let source: Value = serde_json::from_str(GPT6_MODEL_CATALOG).unwrap();
        let models = source["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        for (model, slug) in models.iter().zip(["gpt-6-sol", "gpt-6-luna"]) {
            assert_eq!(model["slug"], slug);
            assert_eq!(model["minimal_client_version"], "0.155.0");
            assert_eq!(model["tool_mode"], "code_mode_only");
            assert_eq!(model["use_responses_lite"], true);
            assert_eq!(
                model["experimental_supported_tools"],
                json!(["send_user_message_async", "clock"])
            );
        }
        let effective_json = profile.effective_catalog_json().unwrap();
        assert_eq!(
            format!("sha256:{:x}", Sha256::digest(effective_json.as_bytes())),
            GPT6_EFFECTIVE_CATALOG_SHA256
        );
        let mut effective: Value = serde_json::from_str(&effective_json).unwrap();
        for model in effective["models"].as_array_mut().unwrap() {
            assert_eq!(model["tool_mode"], "direct");
            assert_eq!(model["experimental_supported_tools"], json!([]));
            model["tool_mode"] = json!("code_mode_only");
            model["experimental_supported_tools"] = json!(["send_user_message_async", "clock"]);
        }
        assert_eq!(effective, source);
    }

    #[test]
    fn model_versions_and_efforts_are_exact() {
        for model in ["gpt-6-sol", "gpt-6-luna"] {
            for effort in ["low", "medium", "high"] {
                let profile =
                    CodexProfile::new_for_version(GPT6_CODEX_VERSION, model, effort).unwrap();
                assert_eq!(profile.version(), GPT6_CODEX_VERSION);
                assert_eq!(
                    profile.turn_start("thread-1", "input").unwrap()["effort"],
                    effort
                );
            }
        }
        assert!(CodexProfile::new_for_version(GPT6_CODEX_VERSION, "gpt-6-sol", "ultra").is_ok());
        assert!(CodexProfile::new_for_version(GPT6_CODEX_VERSION, "gpt-6-luna", "ultra").is_err());
        for (version, model) in [
            (CODEX_VERSION, "gpt-6-sol"),
            (GPT6_CODEX_VERSION, "gpt-5.6-sol"),
            ("0.155.0", "gpt-6-sol"),
            ("0.156.0", "gpt-6-luna"),
        ] {
            assert!(CodexProfile::new_for_version(version, model, "high").is_err());
        }
    }

    #[test]
    fn model_and_effort_are_exact_and_never_fallback() {
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            for effort in ["low", "medium", "high", "xhigh", "max"] {
                let profile = CodexProfile::new(model, effort).unwrap();
                assert_eq!(profile.model(), model);
                assert_eq!(profile.effort(), effort);
                assert_eq!(
                    profile.turn_start("thread-1", "exact input").unwrap()["effort"],
                    effort
                );
            }
        }
        assert!(CodexProfile::new("gpt-5.6-sol", "ultra").is_ok());
        assert!(CodexProfile::new("gpt-5.6-terra", "ultra").is_ok());
        assert!(CodexProfile::new("gpt-5.6-luna", "ultra").is_err());
        for (model, effort) in [
            ("gpt-5.5", "high"),
            ("gpt-5.6-sol-latest", "high"),
            ("GPT-5.6-sol", "high"),
            ("gpt-5.6-sol", "none"),
            ("gpt-5.6-sol", "high "),
        ] {
            assert!(CodexProfile::new(model, effort).is_err());
        }
    }

    #[test]
    fn generated_config_has_only_pinned_provider_and_restrictions() {
        let config: toml::Value = profile()
            .config_toml("/adapter/models.json")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(config["model_provider"].as_str(), Some(PROVIDER_ID));
        let providers = config["model_providers"].as_table().unwrap();
        assert_eq!(providers.len(), 1);
        let provider = &providers[PROVIDER_ID];
        assert_eq!(provider["name"].as_str(), Some("OpenAI"));
        assert_eq!(
            provider["base_url"].as_str(),
            Some("https://chatgpt.com/backend-api/codex")
        );
        assert_eq!(provider["supports_websockets"].as_bool(), Some(false));
        assert_eq!(provider["requires_openai_auth"].as_bool(), Some(true));
        for field in [
            "env_key",
            "experimental_bearer_token",
            "auth",
            "aws",
            "env_http_headers",
        ] {
            assert!(provider.get(field).is_none());
        }
        for feature in [
            "shell_tool",
            "multi_agent",
            "multi_agent_v2",
            "apps",
            "plugins",
            "tool_suggest",
            "image_generation",
            "memories",
            "goals",
            "token_budget",
            "current_time_reminder",
            "deferred_executor",
            "hooks",
            "code_mode",
            "code_mode_only",
            "code_mode_host",
        ] {
            assert_eq!(
                config["features"][feature].as_bool(),
                Some(false),
                "{feature}"
            );
        }
        assert_eq!(config["agents"]["enabled"].as_bool(), Some(false));
        assert_eq!(config["mcp_servers"].as_table().unwrap().len(), 0);
        assert_eq!(
            config["tools"]["update_plan"]["enabled"].as_bool(),
            Some(false)
        );
        assert_eq!(
            config["tools"]["experimental_request_user_input"]["enabled"].as_bool(),
            Some(false)
        );
        for path in [
            "relative.json",
            "/x/../models.json",
            "/x//models.json",
            "/x/\nmodels.json",
        ] {
            assert!(profile().config_toml(path).is_err());
        }
        let quoted = "/adapter/a\"b.json";
        let config: toml::Value = profile().config_toml(quoted).unwrap().parse().unwrap();
        assert_eq!(config["model_catalog_json"].as_str(), Some(quoted));
    }

    #[test]
    fn gpt6_config_and_response_bind_the_new_client_version() {
        let profile =
            CodexProfile::new_for_version(GPT6_CODEX_VERSION, "gpt-6-luna", "high").unwrap();
        let config: toml::Value = profile
            .config_toml("/adapter/models.json")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(config["model"].as_str(), Some("gpt-6-luna"));
        assert_eq!(config["model_reasoning_effort"].as_str(), Some("high"));
        assert_eq!(
            config["model_providers"][PROVIDER_ID]["http_headers"]["version"].as_str(),
            Some(GPT6_CODEX_VERSION)
        );
        assert_eq!(
            config["model_providers"][PROVIDER_ID]["supports_websockets"].as_bool(),
            Some(false)
        );
        assert_eq!(config["features"]["shell_tool"].as_bool(), Some(false));
        assert_eq!(config["features"]["code_mode_only"].as_bool(), Some(false));
        let mut response = thread_response();
        response["model"] = json!("gpt-6-luna");
        response["thread"]["cliVersion"] = json!(GPT6_CODEX_VERSION);
        assert!(profile.validate_thread_start(&response).is_ok());
        response["thread"]["cliVersion"] = json!(CODEX_VERSION);
        assert!(profile.validate_thread_start(&response).is_err());
    }

    #[test]
    fn tools_are_closed_non_deferred_and_match_admitted_operations() {
        let operations = [
            FileOperation::Read,
            FileOperation::Remove,
            FileOperation::Write,
        ];
        let tools = dynamic_tools(&operations).unwrap();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "pillbox_read_file",
                "pillbox_remove_file",
                "pillbox_write_file"
            ]
        );
        for tool in &tools {
            assert_eq!(tool["type"], "function");
            assert_eq!(tool["deferLoading"], false);
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
            assert!(tool.get("namespace").is_none());
        }
        assert_eq!(dynamic_tools(&[FileOperation::Write]).unwrap().len(), 1);
        assert!(dynamic_tools(&[]).unwrap().is_empty());
        assert!(dynamic_tools(&[FileOperation::Read, FileOperation::Read]).is_err());
        assert!(dynamic_tools(&[FileOperation::Write, FileOperation::Read]).is_err());
        let request = profile().thread_start(&operations).unwrap();
        assert_eq!(request["allowProviderModelFallback"], false);
        for field in [
            "environments",
            "runtimeWorkspaceRoots",
            "selectedCapabilityRoots",
        ] {
            assert_eq!(request[field], json!([]));
        }
        assert_eq!(request["dynamicTools"], json!(tools));
        assert!(request.get("config").is_none());
    }

    #[test]
    fn input_is_one_exact_text_item_without_hidden_overrides() {
        let input = "line one\n\"quoted\" and Unicode →\u{0}";
        let params = profile().turn_start("thread-1", input).unwrap();
        assert_eq!(
            params,
            json!({"threadId":"thread-1", "model":"gpt-5.6-sol",
            "effort":"high", "input":[{"type":"text", "text":input, "text_elements":[]}]})
        );
        assert!(profile().turn_start("thread-1", "").is_err());
        assert!(profile()
            .turn_start("thread-1", &"x".repeat(MAX_RENDERED_INPUT_BYTES + 1))
            .is_err());
        assert!(profile().turn_start("wrong\nthread", "input").is_err());
        assert_eq!(initialize_params()["capabilities"]["experimentalApi"], true);
    }

    #[test]
    fn thread_response_rejects_model_policy_and_ambient_context_changes() {
        assert_eq!(
            profile().validate_thread_start(&thread_response()).unwrap(),
            "thread-1"
        );
        for (field, value) in [
            ("model", json!("gpt-5.6-luna")),
            ("modelProvider", json!("openai")),
            ("reasoningEffort", Value::Null),
            ("approvalPolicy", json!("on-request")),
            ("approvalsReviewer", json!("auto_review")),
            ("cwd", json!("/repo")),
            ("runtimeWorkspaceRoots", json!(["/repo"])),
            ("instructionSources", json!(["/repo/AGENTS.md"])),
            ("sandbox", json!({"type":"readOnly", "networkAccess":true})),
        ] {
            let mut response = thread_response();
            response[field] = value;
            assert!(
                profile().validate_thread_start(&response).is_err(),
                "{field}"
            );
        }
        for field in [
            "model",
            "modelProvider",
            "reasoningEffort",
            "runtimeWorkspaceRoots",
            "instructionSources",
        ] {
            let mut response = thread_response();
            response.as_object_mut().unwrap().remove(field);
            assert!(
                profile().validate_thread_start(&response).is_err(),
                "missing {field}"
            );
        }
        let mut response = thread_response();
        response["thread"]["cliVersion"] = json!("0.154.0");
        assert!(profile().validate_thread_start(&response).is_err());
    }

    #[test]
    fn calls_require_correlation_admitted_tool_and_closed_arguments() {
        let read = call("pillbox_read_file", json!({"path":"src/main.rs"}));
        assert_eq!(
            parse_dynamic_call(&read, "thread-1", "turn-1", &[FileOperation::Read], 8).unwrap(),
            DynamicCall {
                call_id: "call-1".into(),
                operation: FileCall::Read {
                    path: "src/main.rs".into()
                }
            }
        );
        assert!(
            parse_dynamic_call(&read, "thread-2", "turn-1", &[FileOperation::Read], 8).is_err()
        );
        assert!(
            parse_dynamic_call(&read, "thread-1", "turn-2", &[FileOperation::Read], 8).is_err()
        );
        assert!(
            parse_dynamic_call(&read, "thread-1", "turn-1", &[FileOperation::Write], 8).is_err()
        );
        for (field, value) in [
            ("tool", json!("exec_command")),
            ("tool", json!("send_user_message_async")),
            ("tool", json!("clock")),
            ("namespace", json!("functions")),
            ("callId", json!("")),
            ("arguments", json!({"path":"x", "extra":true})),
            ("arguments", json!({"path":1})),
            ("arguments", json!("{\"path\":\"x\"}")),
            ("extra", json!(true)),
        ] {
            let mut invalid = read.clone();
            invalid[field] = value;
            assert!(
                parse_dynamic_call(&invalid, "thread-1", "turn-1", &[FileOperation::Read], 8)
                    .is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn writes_decode_binary_with_bounded_closed_inputs() {
        let bytes = [0, 255, 8];
        let write = call(
            "pillbox_write_file",
            json!({"path":"bin", "executable":true,
            "encoding":"base64", "content":BASE64.encode(bytes)}),
        );
        assert_eq!(
            parse_dynamic_call(&write, "thread-1", "turn-1", &[FileOperation::Write], 3)
                .unwrap()
                .operation,
            FileCall::Write {
                path: "bin".into(),
                executable: true,
                bytes: bytes.to_vec()
            }
        );
        assert!(
            parse_dynamic_call(&write, "thread-1", "turn-1", &[FileOperation::Write], 2).is_err()
        );
        for content in ["%%%", "Zm9v!", "====", "YR==", "a".repeat(100).as_str()] {
            let invalid = call(
                "pillbox_write_file",
                json!({"path":"bin", "executable":false, "encoding":"base64", "content":content}),
            );
            assert!(
                parse_dynamic_call(&invalid, "thread-1", "turn-1", &[FileOperation::Write], 3)
                    .is_err()
            );
        }
    }

    #[test]
    fn text_writes_preserve_utf8_bytes_and_require_an_explicit_codec() {
        let text = "→\n\"\\\0";
        let write = call(
            "pillbox_write_file",
            json!({
                "path":"source", "executable":false, "encoding":"utf8", "content":text,
            }),
        );
        let parse = |value: &Value, limit| {
            parse_dynamic_call(value, "thread-1", "turn-1", &[FileOperation::Write], limit)
        };
        assert_eq!(
            parse(&write, text.len() as u64).unwrap().operation,
            FileCall::Write {
                path: "source".into(),
                executable: false,
                bytes: text.as_bytes().to_vec(),
            }
        );
        assert!(parse(&write, text.len() as u64 - 1).is_err());
        for encoding in [json!("UTF-8"), json!("hex"), Value::Null, json!(1)] {
            let mut invalid = write.clone();
            invalid["arguments"]["encoding"] = encoding;
            assert!(parse(&invalid, 64).is_err());
        }
        for field in ["path", "executable", "encoding", "content"] {
            let mut invalid = write.clone();
            invalid["arguments"].as_object_mut().unwrap().remove(field);
            assert!(parse(&invalid, 64).is_err(), "missing {field}");
        }
        let mut legacy = write.clone();
        legacy["arguments"]["contentBase64"] = json!("eA==");
        assert!(parse(&legacy, 64).is_err());
        for encoding in ["utf8", "base64"] {
            let mut empty = write.clone();
            empty["arguments"]["encoding"] = json!(encoding);
            empty["arguments"]["content"] = json!("");
            assert!(
                matches!(parse(&empty, 1).unwrap().operation, FileCall::Write { bytes, .. } if bytes.is_empty())
            );
        }
        let one = call(
            "pillbox_write_file",
            json!({
                "path":"bin", "executable":false, "encoding":"base64", "content":"/w==",
            }),
        );
        assert!(
            matches!(parse(&one, 1).unwrap().operation, FileCall::Write { bytes, .. } if bytes == [255])
        );
    }

    #[test]
    fn terminals_are_correlated_and_success_cannot_carry_an_error() {
        assert_eq!(
            validate_turn_started(
                &json!({"turn":{"id":"turn-1","status":"inProgress","items":[]}})
            )
            .unwrap(),
            "turn-1"
        );
        let mut terminal = json!({"threadId":"thread-1", "turn":{"id":"turn-1","status":"completed","items":[],"error":null}});
        assert_eq!(
            validate_turn_terminal(&terminal, "thread-1", "turn-1").unwrap(),
            TurnTerminal::Completed
        );
        assert!(validate_turn_terminal(&terminal, "other", "turn-1").is_err());
        assert!(validate_turn_terminal(&terminal, "thread-1", "other").is_err());
        terminal["turn"]["error"] = json!({"message":"failure"});
        assert!(validate_turn_terminal(&terminal, "thread-1", "turn-1").is_err());
        terminal["turn"]["status"] = json!("failed");
        assert_eq!(
            validate_turn_terminal(&terminal, "thread-1", "turn-1").unwrap(),
            TurnTerminal::Failed
        );
        terminal["turn"]["error"] = Value::Null;
        assert!(validate_turn_terminal(&terminal, "thread-1", "turn-1").is_err());
        terminal["turn"]["status"] = json!("inProgress");
        assert!(validate_turn_terminal(&terminal, "thread-1", "turn-1").is_err());
    }

    #[test]
    fn unsupported_requests_receive_only_a_bounded_denial() {
        for id in [json!(17), json!("request-17")] {
            let denial = unsupported_server_request(&id).unwrap();
            assert_eq!(denial["id"], id);
            assert_eq!(denial["error"]["code"], -32601);
            assert!(denial.to_string().len() < 200);
        }
        for id in [
            Value::Null,
            json!(false),
            json!(1.5),
            json!({}),
            json!("x".repeat(129)),
        ] {
            assert!(unsupported_server_request(&id).is_err());
        }
    }
}
