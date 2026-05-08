use crate::skill::{self, Skill};
use agentsdk::core::tools::{Tool, ToolExecute};
use p1e_sandbox::SandboxConfig;
use serde_json::json;
use std::collections::HashSet;
use std::fmt::Write;
use std::sync::{Arc, Mutex};

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct LoadSkillsInput {
    /// List of skill names to load (e.g. [`"filesystem"`, `"developer"`])
    skills: Vec<String>,
}

fn extract_string_array(params: &serde_json::Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Load one or more skills by name. Auto-resolves `needs` dependencies.
///
/// When `loaded` is `Some`, already-loaded skills are skipped with a message
/// and newly loaded ones are recorded.
#[allow(clippy::unwrap_used)]
pub fn load_skills_tool(
    registry: Arc<crate::registry::Registry>,
    loaded: Option<Arc<Mutex<HashSet<String>>>>,
) -> Tool {
    Tool::builder()
        .name("load_skills")
        .description("Load skills for provided names")
        .input_schema(schemars::schema_for!(LoadSkillsInput))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            super::emit_tool_input("load_skills", &params);
            let names = extract_string_array(&params, "skills");

            if names.is_empty() {
                return Err("skills parameter must be a non-empty array of skill names".to_string());
            }

            let resolved = Skill::resolve(&registry.skills, &names);
            if resolved.is_empty() {
                return Err("No skills found matching the requested names".to_string());
            }

            let mut output = String::new();
            for skill in &resolved {
                if let Some(ref loaded) = loaded {
                    let mut guard = super::safe_lock(loaded);
                    if guard.contains(&skill.name) {
                        writeln!(
                            output,
                            "Skill '{}' is already loaded — skipping.",
                            skill.name
                        )
                        .ok();
                        continue;
                    }
                    guard.insert(skill.name.clone());
                }
                write!(output, "## Skill: {}\n{}\n---\n", skill.name, skill.content).ok();
            }
            Ok(output)
        }))
        .build()
        .unwrap()
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct LoadReferencesInput {
    /// Skill name whose references to load.
    skill: String,
    /// Reference filenames to load (e.g. [`"checklist.md"`]).
    /// Already-loaded references are skipped automatically.
    references: Vec<String>,
}

/// Validate a filename: no path traversal, absolute paths, or hidden files; must match allowed extensions.
fn validate_filename(name: &str, allowed_exts: &[&str]) -> Result<(), String> {
    if name.contains("..") || name.starts_with('/') || name.starts_with('.') {
        return Err(format!(
            "Invalid filename '{name}': path traversal, absolute paths, and hidden files are not allowed"
        ));
    }
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str());
    let valid = ext.is_some_and(|e| {
        allowed_exts
            .iter()
            .any(|allowed| e.eq_ignore_ascii_case(allowed))
    });
    if !valid {
        let list = allowed_exts.join(", .");
        return Err(format!(
            "Invalid filename '{name}': only .{list} files are allowed"
        ));
    }
    Ok(())
}

/// Load reference files from a skill directory. Tracks what's already loaded.
#[allow(clippy::unwrap_used)]
pub fn load_references_tool(loaded_refs: Arc<Mutex<HashSet<String>>>) -> Tool {
    Tool::builder()
        .name("load_references")
        .description("Load reference files of a skill for extra knowledge")
        .input_schema(schemars::schema_for!(LoadReferencesInput))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            super::emit_tool_input("load_references", &params);
            let Some(skill_name) = params.get("skill").and_then(|v| v.as_str()) else {
                return Err("skill parameter is required".to_string());
            };
            let skill_name = skill_name.to_string();

            let ref_names = extract_string_array(&params, "references");

            if ref_names.is_empty() {
                return Err(
                    "references parameter must be a non-empty array of filenames".to_string(),
                );
            }

            for name in &ref_names {
                validate_filename(name, &["md"])?;
            }

            if !skill::skill_exists(&skill_name) {
                return Err(format!("Skill '{skill_name}' not found"));
            }

            let mut output = String::new();
            for ref_name in &ref_names {
                let key = format!("{skill_name}/{ref_name}");
                if super::safe_lock(&loaded_refs).contains(&key) {
                    writeln!(output, "Reference {key} already loaded — skipping.").ok();
                    continue;
                }
                let Some(content) = skill::load_reference(&skill_name, ref_name) else {
                    writeln!(
                        output,
                        "Error: reference '{ref_name}' not found for skill '{skill_name}'"
                    )
                    .ok();
                    continue;
                };
                write!(output, "### Reference: {key}\n{content}\n---\n").ok();
                super::safe_lock(&loaded_refs).insert(key);
            }
            Ok(output)
        }))
        .build()
        .unwrap()
}

// ── execute_skill_script ──────────────────────────────────────────

const SCRIPT_EXTENSIONS: &[&str] = &["sh", "bash", "py", "js", "ts", "rb", "pl"];

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ExecuteSkillScriptInput {
    skill: String,
    script: String,
    #[serde(default)]
    args: Option<String>,
}

/// Run a command inside the sandbox. Returns JSON with stdout, stderr, exit code.
#[allow(clippy::unwrap_used)]
fn run_sandboxed(cmd: &str, cfg: &SandboxConfig) -> String {
    let output = p1e_sandbox::build_shell_command(cmd, cfg)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("PAGER", "cat")
        .output();
    let (stdout, stderr, code) = match output {
        Ok(out) => (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
            out.status.code().unwrap_or(-1),
        ),
        Err(e) => (String::new(), e.to_string(), -1),
    };
    serde_json::to_string(&json!({
        "cmd": cmd,
        "code": code,
        "stdout": stdout,
        "stderr": stderr,
    }))
    .unwrap_or_default()
}

/// Execute a script from a skill directory. Only filesystem scripts are supported.
#[allow(clippy::unwrap_used)]
pub fn execute_skill_script_tool(sandbox_settings: Arc<SandboxConfig>) -> Tool {
    Tool::builder()
        .name("execute_skill_script")
        .description("Execute a script from a skill directory. Runs inside a sandbox with read access to the skill directory.")
        .input_schema(schemars::schema_for!(ExecuteSkillScriptInput))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            super::emit_tool_input("execute_skill_script", &params);
            let Some(skill_name) = params.get("skill").and_then(|v| v.as_str()) else {
                return Err("skill parameter is required".to_string());
            };
            let skill_name = skill_name.to_string();

            let Some(script_name) = params.get("script").and_then(|v| v.as_str()) else {
                return Err("script parameter is required".to_string());
            };
            let script_name = script_name.to_string();

            validate_filename(&script_name, SCRIPT_EXTENSIONS)?;

            let Some(dir) = skill::skill_dir(&skill_name) else {
                return Err(format!("Skill '{skill_name}' not found"));
            };

            let script_path = dir.join(&script_name);
            if !script_path.exists() {
                return Err(format!(
                    "Script '{script_name}' not found for skill '{skill_name}'"
                ));
            }

            let args = params
                .get("args")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let cmd = if args.is_empty() {
                format!("\"{}\"", script_path.display())
            } else {
                format!("\"{}\" {}", script_path.display(), args)
            };

            let mut sandbox = (*sandbox_settings).clone();
            let skill_path = dir.to_string_lossy().to_string();
            if !sandbox.allow_read.contains(&skill_path) {
                sandbox.allow_read.push(skill_path);
            }

            Ok(run_sandboxed(&cmd, &sandbox))
        }))
        .build()
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::pie_home;
    use agentsdk::core::ToolContext;
    use std::fs;

    /// Create a temporary skill in ~/.pie/skills/ with a SKILL.md and optional script files.
    /// Returns the skill name and a guard that cleans up on drop.
    fn setup_test_skill(scripts: &[(&str, &str)]) -> (String, TempSkillGuard) {
        let id = uuid::Uuid::now_v7();
        let name = format!("_test-{id}");
        let skills_root = pie_home().join("skills");
        let _ = fs::create_dir_all(&skills_root);
        let dir = tempfile::TempDir::new_in(&skills_root).unwrap();
        let tmp = dir.path();
        fs::write(
            tmp.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test\n---\nTest skill"),
        )
        .unwrap();
        for (filename, content) in scripts {
            let path = tmp.join(filename);
            fs::write(&path, content).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let skill_path = skills_root.join(&name);
        let _ = fs::remove_dir_all(&skill_path);
        fs::rename(tmp, &skill_path).unwrap();
        // Forget the TempDir so it doesn't try to clean up the (now-moved) random dir
        // and let our guard handle the named path instead.
        let _ = dir.keep();
        (name, TempSkillGuard { path: skill_path })
    }

    /// RAII guard that removes a test skill directory on drop.
    struct TempSkillGuard {
        path: std::path::PathBuf,
    }

    impl Drop for TempSkillGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn call_execute(params: serde_json::Value) -> Result<String, String> {
        let tool = execute_skill_script_tool(Arc::new(SandboxConfig::default()));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(tool.execute.call(ToolContext::default(), params))
            .map_err(|e| e.to_string())
    }

    #[test]
    fn execute_runs_filesystem_script() {
        let (name, _guard) =
            setup_test_skill(&[("echo.sh", "#!/bin/bash\necho hello-from-test\n")]);
        let result = call_execute(json!({
            "skill": name,
            "script": "echo.sh"
        }));
        let out = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["code"], 0);
        assert_eq!(parsed["stdout"], "hello-from-test");
    }

    #[test]
    fn execute_reports_nonzero_exit() {
        let (name, _guard) =
            setup_test_skill(&[("fail.sh", "#!/bin/bash\necho oops >&2\nexit 1\n")]);
        let result = call_execute(json!({
            "skill": name,
            "script": "fail.sh"
        }));
        let out = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["code"], 1);
        assert_eq!(parsed["stderr"], "oops");
    }

    #[test]
    fn execute_passes_args() {
        let (name, _guard) = setup_test_skill(&[("args.sh", "#!/bin/bash\necho \"$1\" \"$2\"\n")]);
        let result = call_execute(json!({
            "skill": name,
            "script": "args.sh",
            "args": "foo bar"
        }));
        let out = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["code"], 0);
        assert_eq!(parsed["stdout"], "foo bar");
    }

    #[test]
    fn execute_rejects_path_traversal() {
        let result = call_execute(json!({
            "skill": "anything",
            "script": "../etc/passwd"
        }));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("path traversal"));
    }

    #[test]
    fn execute_rejects_wrong_extension() {
        let result = call_execute(json!({
            "skill": "anything",
            "script": "evil.exe"
        }));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("only ."));
    }

    #[test]
    fn execute_rejects_missing_skill() {
        let result = call_execute(json!({
            "skill": "nonexistent-skill-xyz-999",
            "script": "test.sh"
        }));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn execute_rejects_missing_script() {
        let (name, _guard) = setup_test_skill(&[]);
        let result = call_execute(json!({
            "skill": name,
            "script": "nonexistent.sh"
        }));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn validate_filename_rejects_hidden() {
        assert!(validate_filename(".hidden.sh", SCRIPT_EXTENSIONS).is_err());
    }

    #[test]
    fn validate_filename_rejects_absolute() {
        assert!(validate_filename("/etc/sh", SCRIPT_EXTENSIONS).is_err());
    }

    #[test]
    fn validate_filename_accepts_valid_script() {
        assert!(validate_filename("test.sh", SCRIPT_EXTENSIONS).is_ok());
        assert!(validate_filename("run.py", SCRIPT_EXTENSIONS).is_ok());
    }
}
