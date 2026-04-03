use aisdk::core::tools::Tool;
use aisdk::macros::tool;
use serde_json::json;
use std::process::Command;

/// Execute a bash command and return stdout, stderr, and exit code
#[tool]
pub fn bash(cmd: String) -> Tool {
    let result = execute_bash(&cmd);
    Ok(serde_json::to_string(&result).unwrap_or_default())
}

/// Execute a bash command and return structured JSON result.
pub fn execute_bash(cmd: &str) -> serde_json::Value {
    let output = Command::new("sh").arg("-c").arg(cmd).output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let exit_code = out.status.code().unwrap_or(-1);
            json!({
                "cmd": cmd,
                "exitCode": exit_code,
                "stdout": stdout,
                "stderr": stderr,
                "success": exit_code == 0
            })
        }
        Err(e) => json!({
            "cmd": cmd,
            "exitCode": -1,
            "stdout": "",
            "stderr": e.to_string(),
            "success": false
        }),
    }
}
