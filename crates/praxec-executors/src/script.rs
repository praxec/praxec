//! SPEC §22 — the `script` executor kind. Materializes a curated, hash-
//! pinned script body to a per-invocation temp file and execs it.
//!
//! Invocation shape in YAML:
//! ```yaml
//! transitions:
//!   build:
//!     executor:
//!       kind: script
//!       subject: build.cargo.release            # required
//!       args: ["--features=integration"]        # optional, templated
//!       workingDirectory: /path/to/repo         # optional
//!       env: { CI: "true" }                     # optional
//!       treatNonZeroAsFailure: true             # optional (default true)
//! ```
//!
//! Resolution chain:
//!
//! 1. Look up `subject` in the instance's `definition._scriptsLibrary` —
//!    stamped at config-load time by `stamp_scripts_library` (SPEC §22 / N).
//! 2. Write body to a temp file (`tempfile::NamedTempFile`), chmod 0700.
//! 3. Render `args` against `{$.arguments, $.context, $.input}` scopes
//!    (same templating as the cli executor).
//! 4. Exec: if body starts with `#!`, invoke the path directly (kernel
//!    honors shebang); otherwise default to `bash <path>`.
//! 5. Capture stdout/stderr/exit. Stdout auto-parses as JSON when valid.
//! 6. Emit `script_output` Evidence with body hash for audit replay.
//!
//! The temp file is dropped (and deleted) when execution returns. The
//! workflow's transition record carries the script's `subject` + `hash`
//! via the executor output JSON, so a future replay can pull the same
//! script body out of cold storage by hash.
//!
//! ## Oversized args (finding #15)
//!
//! Linux caps a single execve argv string at MAX_ARG_STRLEN (~128 KiB), so
//! a rendered arg beyond that makes the child unspawnable ("Argument list
//! too long"). `concat` fan-in routinely produces such args. Any rendered
//! arg over [`ARG_SPILL_THRESHOLD_BYTES`] is therefore spilled to a temp
//! file (same lifecycle as the body temp file: deleted when execution
//! returns) and its argv slot is replaced with `@argfile:<path>`. The
//! child additionally receives `PRAXEC_SPILLED_ARGS`, a JSON array of the
//! 0-based indices of spilled args (`[]` when none), so a script can
//! distinguish a spill from a literal `@argfile:...` string
//! deterministically.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{Evidence, ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;
use praxec_core::sandbox::{ConfinementProfile, SandboxProvider, SandboxSpec};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::process::Command;
use uuid::Uuid;

/// Finding #15 — Linux caps a single execve argv string at MAX_ARG_STRLEN
/// (32 pages = 128 KiB on the common 4 KiB-page kernel). A rendered arg AT or
/// beyond this makes the child unspawnable with E2BIG ("Argument list too
/// long"). We spill ONLY args that would actually break execve: anything
/// strictly smaller passes verbatim exactly as it did before finding #15, so
/// no historically-working arg silently turns into a literal `@argfile:...`
/// string. Args at/over the limit would have hard-failed E2BIG regardless, so
/// diverting them to a tempfile (and requiring the receiving script to be
/// spill-aware via PRAXEC_SPILLED_ARGS) is strictly better than a crash.
const ARG_SPILL_THRESHOLD_BYTES: usize = 128 * 1024;

/// SPEC §22 + ADR-0006. Confinement is opt-in per script: with no provider and
/// no profile, scripts run exactly as before. A configured provider confines
/// scripts that declare a `confinement` profile; `require_confinement` makes a
/// missing profile a fail-fast error rather than a silent unconfined run.
#[derive(Default)]
pub struct ScriptExecutor {
    provider: Option<Arc<dyn SandboxProvider>>,
    require_confinement: bool,
}

impl ScriptExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Route scripts that declare a `confinement` profile through this sandbox
    /// provider (ADR-0006). Without it, a profiled script fails fast rather than
    /// running unconfined.
    pub fn with_sandbox(mut self, provider: Arc<dyn SandboxProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Strict mode (`praxec.execution.require_confinement`): every script MUST
    /// declare a confinement profile; an unprofiled script fails fast.
    pub fn require_confinement(mut self, yes: bool) -> Self {
        self.require_confinement = yes;
        self
    }
}

#[async_trait]
impl Executor for ScriptExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let cfg = &request.executor_config;

        // Subject lookup is required — there's no "inline body" path on
        // the executor itself (that's what the cli executor is for).
        let subject = cfg.get("subject").and_then(Value::as_str).ok_or_else(|| {
            ExecutorError::Permanent(
                "INVALID_SCRIPT_INVOCATION: script executor requires `subject` \
                     (the curated script's dotted name from the top-level `scripts:` block)"
                    .into(),
            )
        })?;

        // SPEC §22 + N — body lives in the instance's stamped library, not
        // in the live config. This is the §8.2 invariant: an in-flight
        // instance sees the body that existed at workflow.start.
        let lib_pointer = format!(
            "/_scriptsLibrary/{}",
            subject.replace('~', "~0").replace('/', "~1")
        );
        let entry = request
            .workflow
            .definition
            .pointer(&lib_pointer)
            .ok_or_else(|| {
                ExecutorError::Permanent(format!(
                    "SCRIPT_NOT_IN_SNAPSHOT: script '{subject}' not found in this workflow's \
                     `_scriptsLibrary`. Likely cause: the script subject was not collected by \
                     `collect_referenced_script_subjects` at config-load time. Verify the \
                     `executor: {{ kind: script, subject: {subject} }}` reference is on a \
                     transition (not a free-form field) so stamp_scripts_library can find it."
                ))
            })?;
        let body = entry.get("body").and_then(Value::as_str).ok_or_else(|| {
            ExecutorError::Permanent(format!(
                "SCRIPT_NOT_IN_SNAPSHOT: script '{subject}' entry has no `body`. \
                     The snapshot is malformed (likely a bug — file an issue)."
            ))
        })?;
        let hash = entry
            .get("hash")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExecutorError::Permanent(format!(
                    "SCRIPT_NOT_IN_SNAPSHOT: script '{subject}' entry has no `hash`. \
                     The snapshot is malformed (likely a bug — file an issue)."
                ))
            })?
            .to_string();

        // Materialize body → temp file → chmod 0700 → exec.
        //
        // `into_temp_path()` is critical: it drops the open `File` handle
        // while keeping the path (and a Drop guard that deletes the file
        // when the TempPath goes out of scope). Without it, the kernel
        // refuses to exec a file with a writable open handle ("Text file
        // busy", ETXTBSY) on Linux.
        let temp = NamedTempFile::new().map_err(|e| {
            ExecutorError::Connection(format!("failed to create temp file for script: {e}"))
        })?;
        std::fs::write(temp.path(), body)
            .map_err(|e| ExecutorError::Connection(format!("failed to write script body: {e}")))?;
        let temp_path = temp.into_temp_path();
        #[cfg(unix)]
        {
            std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o700)).map_err(
                |e| ExecutorError::Connection(format!("failed to chmod 0700 script file: {e}")),
            )?;
        }

        // Render args from {$.arguments, $.context, $.input} scopes.
        let raw_args = cfg
            .get("args")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let rendered_args: Vec<String> = raw_args
            .iter()
            .map(|a| crate::arg_render::render_arg(a, &request))
            .collect::<Result<Vec<_>, _>>()?;

        // Finding #15 — spill oversized rendered args to temp files so the
        // child stays spawnable under MAX_ARG_STRLEN. Spill files share the
        // body temp file's lifecycle: the `TempPath` guards live until this
        // function returns, then delete the files. Spilled slots become
        // `@argfile:<path>`; `spilled_indices` feeds PRAXEC_SPILLED_ARGS so
        // scripts can tell a spill from a literal `@argfile:...` string.
        let mut spill_paths: Vec<tempfile::TempPath> = Vec::new();
        let mut spilled_indices: Vec<usize> = Vec::new();
        let rendered_args: Vec<String> = rendered_args
            .into_iter()
            .enumerate()
            .map(|(i, arg)| {
                // `<` not `<=`: an arg of exactly MAX_ARG_STRLEN bytes still
                // overflows once the NUL terminator is counted, so it must
                // spill too.
                if arg.len() < ARG_SPILL_THRESHOLD_BYTES {
                    return Ok(arg);
                }
                let spill = NamedTempFile::new().map_err(|e| {
                    ExecutorError::Connection(format!(
                        "failed to create temp file for oversized arg {i}: {e}"
                    ))
                })?;
                std::fs::write(spill.path(), &arg).map_err(|e| {
                    ExecutorError::Connection(format!(
                        "failed to write oversized arg {i} to temp file: {e}"
                    ))
                })?;
                let path = spill.into_temp_path();
                let argv_value = format!("@argfile:{}", path.to_string_lossy());
                spill_paths.push(path);
                spilled_indices.push(i);
                Ok(argv_value)
            })
            .collect::<Result<Vec<_>, ExecutorError>>()?;

        // Honor shebang if body starts with `#!`. Otherwise default to bash.
        // Compute (program, argv) + env + workdir as VALUES so the same intent
        // feeds either the direct (unconfined) path or the sandbox provider.
        let body_has_shebang = body.starts_with("#!");
        let program = if body_has_shebang {
            temp_path.to_string_lossy().into_owned()
        } else {
            // No shebang: invoke through bash. Operators on shells without
            // bash can declare a shebang in their script bodies.
            "bash".to_string()
        };
        let mut argv: Vec<String> = Vec::new();
        if !body_has_shebang {
            argv.push(temp_path.to_string_lossy().into_owned());
        }
        argv.extend(rendered_args.iter().cloned());

        // `workingDirectory` resolves `$.` path expressions (against the same
        // {$.arguments, $.context, $.workflow.input} scopes as `args`), so a
        // flow can point a script leaf at the repo it operates on — e.g.
        // `workingDirectory: "$.workflow.input.repo_path"` for a cargo build/
        // verify step. A plain (non-`$.`) string passes through as a literal
        // path. Resolving here is what lets the build loop run cargo *in the
        // built repo* declaratively instead of in the gateway's CWD.
        let workdir: Option<String> = match cfg.get("workingDirectory") {
            Some(v) => Some(crate::arg_render::render_arg(v, &request)?),
            None => None,
        };

        // Env the script should see: operator-declared `env`, the idempotency
        // key, and praxec's self-identification vars.
        let mut env: Vec<(String, String)> = Vec::new();
        if let Some(extra) = cfg.get("env").and_then(Value::as_object) {
            for (k, v) in extra {
                if let Some(s) = v.as_str() {
                    env.push((k.clone(), s.to_string()));
                }
            }
        }
        if let Some(key) = &request.idempotency_key {
            env.push(("IDEMPOTENCY_KEY".to_string(), key.clone()));
        }
        env.push(("PRAXEC_SCRIPT_SUBJECT".to_string(), subject.to_string()));
        env.push(("PRAXEC_SCRIPT_HASH".to_string(), hash.clone()));
        // Finding #15 — always set (as `[]` when nothing spilled) so scripts
        // can detect spills deterministically.
        env.push((
            "PRAXEC_SPILLED_ARGS".to_string(),
            json!(spilled_indices).to_string(),
        ));

        // ADR-0006 — per-script confinement decision. Default is unconfined
        // (existing scripts unchanged); a declared profile routes through the
        // sandbox; every other shape fails fast naming the script.
        let profile = ConfinementProfile::from_executor_config(cfg)
            .map_err(|e| ExecutorError::Permanent(format!("{e} (script '{subject}')")))?;

        let (stdout, stderr, exit_code, success) = match (&profile, &self.provider) {
            // Confined: hand the built command to the sandbox provider.
            (Some(p), Some(provider)) => {
                let mut command = vec![program.clone()];
                command.extend(argv.iter().cloned());
                let spec = SandboxSpec {
                    workspace: workdir.as_ref().map(PathBuf::from),
                    command,
                    // The script body (and any spilled-arg files) live in
                    // temp files the fresh sandbox tmpfs would otherwise
                    // hide — bind them in read-only.
                    ro_binds: std::iter::once(temp_path.to_path_buf())
                        .chain(spill_paths.iter().map(|p| p.to_path_buf()))
                        .collect(),
                    env: env.clone(),
                    egress: p.egress.clone(),
                    env_allowlist: p.env_allowlist.clone(),
                    limits: p.limits.clone(),
                };
                let out = provider.run(&spec).await.map_err(|e| {
                    ExecutorError::Connection(format!(
                        "sandbox run failed for script '{subject}': {e}"
                    ))
                })?;
                (out.stdout, out.stderr, out.code, out.success)
            }
            // Profiled but no provider configured — refuse, never run unconfined.
            (Some(_), None) => {
                return Err(ExecutorError::Permanent(format!(
                    "CONFINEMENT_UNAVAILABLE: script '{subject}' declares a `confinement` \
                     profile but no sandbox provider is configured — refusing to run it \
                     unconfined. Configure a sandbox provider or remove the profile."
                )));
            }
            // Strict mode + no profile — refuse, never silently run unconfined.
            (None, _) if self.require_confinement => {
                return Err(ExecutorError::Permanent(format!(
                    "CONFINEMENT_REQUIRED: strict mode (praxec.execution.require_confinement) \
                     — script '{subject}' must declare a `confinement` profile."
                )));
            }
            // Unconfined — today's direct path, byte-for-byte behavior.
            (None, _) => {
                let mut cmd = Command::new(&program);
                cmd.args(&argv);
                if let Some(wd) = &workdir {
                    cmd.current_dir(wd);
                }
                for (k, v) in &env {
                    cmd.env(k, v);
                }

                // ETXTBSY retry loop. Linux returns ETXTBSY ("Text file busy",
                // errno 26) when execve targets an inode any process holds open
                // for writing — a concurrent thread's fork() can briefly inherit
                // our fd between fork and the CLOEXEC sweep. Small backoff retry.
                #[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
                const ETXTBSY_ERRNO: i32 = 26;
                let output = {
                    let mut attempt = 0u32;
                    loop {
                        match cmd.output().await {
                            Ok(out) => break out,
                            #[cfg(any(
                                target_os = "linux",
                                target_os = "macos",
                                target_os = "freebsd"
                            ))]
                            Err(e) if e.raw_os_error() == Some(ETXTBSY_ERRNO) && attempt < 5 => {
                                attempt += 1;
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    10 * (1 << attempt),
                                ))
                                .await;
                                continue;
                            }
                            Err(e) => {
                                return Err(ExecutorError::Connection(format!(
                                    "script spawn failed ({program}): {e}"
                                )));
                            }
                        }
                    }
                };
                (
                    output.stdout,
                    output.stderr,
                    output.status.code(),
                    output.status.success(),
                )
            }
        };

        let stdout_str = String::from_utf8_lossy(&stdout).to_string();
        let parsed_json: Value = serde_json::from_str(stdout_str.trim()).unwrap_or(Value::Null);

        // Audit-grade output: includes scriptSubject + scriptHash so a
        // transition record uniquely identifies what code ran, and a
        // future replay can pull the body by hash from cold storage.
        let result = json!({
            "exitCode":      exit_code,
            "success":       success,
            "stdout":        stdout_str,
            "stderr":        String::from_utf8_lossy(&stderr).to_string(),
            "json":          parsed_json,
            "scriptSubject": subject,
            "scriptHash":    hash,
        });

        let treat_nonzero_as_failure = cfg
            .get("treatNonZeroAsFailure")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if treat_nonzero_as_failure && !success {
            return Err(ExecutorError::Permanent(format!(
                "script '{subject}' exited with code {exit_code:?}"
            )));
        }

        Ok(ExecuteResult {
            output: result,
            evidence: vec![Evidence {
                kind: "script_output".to_string(),
                id: Uuid::new_v4().to_string(),
                uri: None,
                summary: Some(format!("Executed script '{subject}'")),
                digest: Some(hash),
                confidence: None,
            }],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}
