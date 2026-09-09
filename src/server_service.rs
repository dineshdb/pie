//! The `pie server` daemon (A2A) as a login service, and its credentials.
//!
//! `pie server token` shows the bearer key clients must send; `pie server
//! install` registers a launchd agent (macOS) or a systemd user unit
//! (Linux) that keeps the daemon running, generating the api key first if
//! the target bind needs one. Both are thin file-writers plus the platform
//! service-manager CLI — no daemonization logic of our own.

use anyhow::{Context, anyhow};
use pie_core::config::{ServerConfig, pie_home};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const LABEL: &str = "io.github.dineshdb.pie.server";
const SERVICE: &str = "pie-server";
const DEFAULT_PATH_ENV: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Print the bearer token clients must send. Intended for scripting:
/// `PIE_TOKEN=$(pie server token)`.
pub fn show_token(server: &ServerConfig) {
    if let Some(key) = &server.api_key {
        println!("{}", key.expose_secret());
    } else {
        println!("no api key configured ([server] api_key in pie.toml).");
        println!(
            "Loopback binds work without one; for remote access, \
             `pie server install --bind <addr>` generates a key automatically."
        );
    }
}

/// Register the daemon with the platform's service manager so it starts at
/// login and restarts on failure. A non-loopback bind requires an api key;
/// one is generated and persisted into `~/.pie/pie.toml` when missing.
/// `hosts` are the hostnames remote clients will use — persisted as
/// `[server] allowed_hosts`, the transport's Host-header allowlist.
pub fn install(
    bind_override: Option<String>,
    hosts: &[String],
    server: &ServerConfig,
) -> anyhow::Result<()> {
    let bind = bind_override.unwrap_or_else(|| server.bind.clone());
    let loopback_bind = ServerConfig {
        bind: bind.clone(),
        ..ServerConfig::default()
    }
    .is_loopback_bind();

    // Values to persist into [server]: a generated api key and/or the
    // remote hostnames. Already-configured values are left untouched.
    let mut persist: Vec<(&str, String)> = Vec::new();
    let token = if loopback_bind {
        server.api_key.as_ref().map(|k| k.expose_secret().clone())
    } else if let Some(k) = &server.api_key {
        Some(k.expose_secret().clone())
    } else {
        let generated = uuid::Uuid::new_v4().to_string();
        persist.push(("api_key", format!("\"{generated}\"")));
        Some(generated)
    };
    if !hosts.is_empty() {
        let listed = hosts
            .iter()
            .map(|host| format!("\"{host}\""))
            .collect::<Vec<_>>()
            .join(", ");
        persist.push(("allowed_hosts", format!("[{listed}]")));
    }
    if !persist.is_empty() {
        let path = write_server_config_values(&config_path(), &persist)
            .context("persisting [server] settings")?;
        println!(
            "stored {} in {}",
            persist
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", "),
            path.display()
        );
    }

    let binary = std::env::current_exe().context("resolving the pie binary")?;
    if binary.components().any(|c| c.as_os_str() == "target") {
        println!(
            "note: registering the development binary {}. \
             For a stable service, run `just install` and reinstall the service.",
            binary.display()
        );
    }
    let log = pie_home().join("logs");
    std::fs::create_dir_all(&log).context("creating the log directory")?;

    if std::cfg!(target_os = "macos") {
        install_launchd(&binary, &bind, &log.join("server.log"))?;
    } else {
        install_systemd(&binary, &bind)?;
    }

    println!("\npie server service installed: http://{bind}");
    match token {
        Some(token) => {
            println!("  api key:   {token}");
            println!("  client header: \"Authorization\": \"Bearer {token}\"");
        }
        None => println!("  api key:   none required (loopback bind)"),
    }
    Ok(())
}

/// Stop and remove the installed service.
pub fn uninstall() -> anyhow::Result<()> {
    if std::cfg!(target_os = "macos") {
        uninstall_launchd()?;
    } else {
        uninstall_systemd()?;
    }
    println!("pie server service removed.");
    Ok(())
}

fn user_id() -> anyhow::Result<String> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .context("running `id -u`")?;
    String::from_utf8(output.stdout)
        .context("`id -u` output")
        .map(|s| s.trim().to_string())
}

// ── macOS: launchd agent ───────────────────────────────────────────

fn plist_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn plist_xml(binary: &Path, bind: &str, log: &Path) -> String {
    let home = dirs::home_dir().unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
      <string>{}</string>
      <string>server</string>
      <string>--bind</string>
      <string>{bind}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{}</string>
    <key>StandardErrorPath</key>
    <string>{}</string>
    <key>EnvironmentVariables</key>
    <dict>
      <key>HOME</key>
      <string>{}</string>
      <key>PATH</key>
      <string>{DEFAULT_PATH_ENV}</string>
    </dict>
  </dict>
</plist>
"#,
        binary.display(),
        log.display(),
        log.display(),
        home.display()
    )
}

fn install_launchd(binary: &Path, bind: &str, log: &Path) -> anyhow::Result<()> {
    let uid = user_id()?;
    let path = plist_path();
    std::fs::create_dir_all(path.parent().context("plist parent")?)?;
    // A stale load would make bootstrap fail with "already bootstrapped".
    // `No such process` is the normal "wasn't loaded" answer; silence it.
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LABEL}")])
        .output();
    // bootout acks before launchd finishes tearing the service down; a
    // bootstrap that races the teardown fails with EIO. Wait until the
    // label is really gone.
    for _ in 0..20 {
        let gone = Command::new("launchctl")
            .args(["print", &format!("gui/{uid}/{LABEL}")])
            .output()
            .is_ok_and(|out| !out.status.success());
        if gone {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    std::fs::write(&path, plist_xml(binary, bind, log))?;
    let status = Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{uid}"), &path.to_string_lossy()])
        .status()
        .context("running launchctl bootstrap")?;
    if !status.success() {
        return Err(anyhow!(
            "launchctl bootstrap failed; see `launchctl print gui/{uid}/{LABEL}`"
        ));
    }
    Ok(())
}

fn uninstall_launchd() -> anyhow::Result<()> {
    let uid = user_id()?;
    let path = plist_path();
    let status = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LABEL}")])
        .status()
        .context("running launchctl bootout")?;
    if !path.exists() {
        if status.success() {
            return Ok(());
        }
        return Err(anyhow!("service {LABEL} is not installed"));
    }
    std::fs::remove_file(&path)?;
    Ok(())
}

// ── Linux: systemd user unit ───────────────────────────────────────

fn unit_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config/systemd/user")
        .join(format!("{SERVICE}.service"))
}

fn systemd_unit(binary: &Path, bind: &str) -> String {
    format!(
        r"[Unit]
Description=pie A2A server
After=network-online.target

[Service]
ExecStart={} server --bind {bind}
Environment=PATH={DEFAULT_PATH_ENV}
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
",
        binary.display()
    )
}

fn install_systemd(binary: &Path, bind: &str) -> anyhow::Result<()> {
    let path = unit_path();
    std::fs::create_dir_all(path.parent().context("unit parent")?)?;
    std::fs::write(&path, systemd_unit(binary, bind))?;
    let steps: [&[&str]; 2] = [
        &["--user", "daemon-reload"],
        &["--user", "enable", "--now", SERVICE],
    ];
    for args in steps {
        let status = Command::new("systemctl")
            .args(args)
            .status()
            .context("running systemctl")?;
        if !status.success() {
            return Err(anyhow!("systemctl {} failed", args.join(" ")));
        }
    }
    Ok(())
}

fn uninstall_systemd() -> anyhow::Result<()> {
    let path = unit_path();
    if !path.exists() {
        return Err(anyhow!("service {SERVICE} is not installed"));
    }
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", SERVICE])
        .status();
    std::fs::remove_file(&path)?;
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    Ok(())
}

// ── api key persistence ────────────────────────────────────────────

fn config_path() -> PathBuf {
    pie_home().join("pie.toml")
}

/// Store `[server]` values in the user's pie.toml: each `(name, toml_value)`
/// replaces an existing key, is inserted into an existing `[server]`
/// section, or lands in an appended section. Everything else is preserved
/// verbatim.
fn write_server_config_values(path: &Path, values: &[(&str, String)]) -> anyhow::Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(String::from).collect();
    let mut pending: Vec<(&str, String)> = values.to_vec();

    if let Some(start) = lines.iter().position(|line| line.trim() == "[server]") {
        // The section runs to the next header (or end of file).
        let mut end = lines.len();
        for (offset, line) in lines.iter().enumerate().skip(start + 1) {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                end = offset;
                break;
            }
        }
        // Replace present keys in place; collect the rest for insertion.
        let mut i = start + 1;
        while i < end {
            let name = lines
                .get(i)
                .and_then(|line| line.split('=').next())
                .map(str::trim)
                .map(String::from);
            if let Some(pos) = pending
                .iter()
                .position(|(wanted, _)| Some((*wanted).to_string()) == name)
                && let Some(slot) = lines.get_mut(i)
            {
                let (name, value) = pending.remove(pos);
                *slot = format!("{name} = {value}");
            }
            i += 1;
        }
        if !pending.is_empty() {
            // Insert after the section's last non-empty line, so a blank
            // line before the next header stays where it was.
            let insert_at = (start + 1..end)
                .rev()
                .find(|&i| lines.get(i).is_some_and(|line| !line.trim().is_empty()))
                .map_or(start + 1, |i| i + 1);
            for (offset, (name, value)) in pending.iter().enumerate() {
                lines.insert(insert_at + offset, format!("{name} = {value}"));
            }
        }
    } else {
        lines.push(String::new());
        lines.push("[server]".into());
        for (name, value) in &pending {
            lines.push(format!("{name} = {value}"));
        }
    }

    let mut file = std::fs::File::create(path)?;
    file.write_all(lines.join("\n").as_bytes())?;
    file.write_all(b"\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = file.metadata()?.permissions();
        // The config now (or already) carries a credential: keep it private.
        perms.set_mode(0o600);
        file.set_permissions(perms)?;
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn api_key_upsert_appends_section_to_config_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pie.toml");
        std::fs::write(
            &path,
            "log_level = \"info\"\n\n[sandbox]\nallow_write = [\".\"]\n",
        )
        .unwrap();

        let written =
            write_server_config_values(&path, &[("api_key", "\"key-1\"".into())]).unwrap();
        assert_eq!(written, path);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[server]\napi_key = \"key-1\""));
        // Existing sections are preserved verbatim, before the new one.
        assert!(contents.contains("[sandbox]\nallow_write = [\".\"]"));
    }

    #[test]
    fn api_key_upsert_inserts_into_existing_empty_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pie.toml");
        std::fs::write(&path, "[server]\nbind = \"127.0.0.1:1\"\n\n[sandbox]\n").unwrap();

        write_server_config_values(&path, &[("api_key", "\"key-2\"".into())]).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        // The key joins the existing server section, before the next header.
        assert!(
            contents.contains("[server]\nbind = \"127.0.0.1:1\"\napi_key = \"key-2\"\n\n[sandbox]")
        );
    }

    #[test]
    fn api_key_upsert_replaces_existing_key_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pie.toml");
        std::fs::write(
            &path,
            "[server]\nbind = \"127.0.0.1:1\"\napi_key = \"old\"\n\n[sandbox]\n",
        )
        .unwrap();

        write_server_config_values(&path, &[("api_key", "\"new-key\"".into())]).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("api_key = \"new-key\""));
        assert!(!contents.contains("old"));
        assert!(contents.contains("bind = \"127.0.0.1:1\""));
        assert_eq!(contents.matches("api_key").count(), 1);
    }

    #[test]
    fn api_key_upsert_replaces_key_before_next_section() {
        // [server] followed by another section: the new key must land in
        // the server section, not at the end of the file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pie.toml");
        std::fs::write(&path, "[server]\n\n[sandbox]\n").unwrap();

        write_server_config_values(&path, &[("api_key", "\"key-3\"".into())]).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[server]\napi_key = \"key-3\"\n\n[sandbox]"));
    }

    #[test]
    fn server_config_upsert_writes_multiple_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pie.toml");
        std::fs::write(&path, "[server]\nbind = \"127.0.0.1:1\"\n\n[sandbox]\n").unwrap();

        write_server_config_values(
            &path,
            &[
                ("api_key", "\"tok\"".into()),
                ("allowed_hosts", "[\"citadel.lvh.me\"]".into()),
            ],
        )
        .unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains(
                "[server]\nbind = \"127.0.0.1:1\"\napi_key = \"tok\"\nallowed_hosts = [\"citadel.lvh.me\"]"
            ),
            "{contents}"
        );

        // A second write replaces the existing values in place.
        write_server_config_values(&path, &[("allowed_hosts", "[\"other.host\"]".into())]).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("allowed_hosts = [\"other.host\"]"));
        assert_eq!(contents.matches("allowed_hosts").count(), 1);
    }

    #[test]
    fn plist_and_unit_carry_binary_bind_and_path() {
        let plist = plist_xml(
            Path::new("/usr/local/bin/pie"),
            "0.0.0.0:8629",
            Path::new("/logs/server.log"),
        );
        assert!(plist.contains("<string>/usr/local/bin/pie</string>"));
        assert!(plist.contains("<string>server</string>"));
        assert!(plist.contains("<string>0.0.0.0:8629</string>"));
        assert!(plist.contains("<string>/logs/server.log</string>"));
        assert!(plist.contains(DEFAULT_PATH_ENV));

        let unit = systemd_unit(Path::new("/usr/local/bin/pie"), "0.0.0.0:8629");
        assert!(unit.contains("ExecStart=/usr/local/bin/pie server --bind 0.0.0.0:8629"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains(DEFAULT_PATH_ENV));
    }
}
