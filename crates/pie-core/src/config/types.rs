use crate::utils::output::OutputFormat;
use clap::Args;
use p1e_sandbox::SandboxConfig;
use redact::Secret;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use url::Url;

#[derive(Debug, Clone, Default, Args)]
pub struct ProviderEndpoint {
    /// Well-known provider name (config only)
    #[arg(skip)]
    pub name: Option<String>,

    /// OpenAI-compatible base URL
    #[arg(long, alias = "openai-url", alias = "base-url", global = true)]
    pub openai: Option<String>,

    /// Anthropic API base URL
    #[arg(long, alias = "anthropic-url", global = true)]
    pub anthropic: Option<String>,
}

impl Serialize for ProviderEndpoint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if self.openai.is_some() || self.anthropic.is_some() {
            CustomEndpoint {
                openai: self.openai.clone(),
                anthropic: self.anthropic.clone(),
            }
            .serialize(serializer)
        } else {
            self.name
                .as_deref()
                .unwrap_or("default")
                .serialize(serializer)
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ProviderEndpointDto {
    Name(String),
    Custom(CustomEndpoint),
}

#[derive(Serialize, Deserialize)]
struct CustomEndpoint {
    #[serde(alias = "base_url", alias = "endpoint", alias = "openai_url")]
    openai: Option<String>,
    #[serde(rename = "anthropic_url")]
    anthropic: Option<String>,
}

impl<'de> Deserialize<'de> for ProviderEndpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match ProviderEndpointDto::deserialize(deserializer)? {
            ProviderEndpointDto::Name(n) => Ok(ProviderEndpoint {
                name: Some(n),
                ..Default::default()
            }),
            ProviderEndpointDto::Custom(CustomEndpoint { openai, anthropic }) => {
                Ok(ProviderEndpoint {
                    name: None,
                    openai,
                    anthropic,
                })
            }
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, Args)]
pub struct ProviderBaseUrl {
    /// OpenAI-compatible base URL
    #[arg(long, alias = "openai-url", alias = "base-url")]
    #[serde(rename = "openai_url", alias = "base_url")]
    pub openai: Option<String>,

    /// Anthropic API base URL
    #[arg(long, alias = "anthropic-url")]
    #[serde(rename = "anthropic_url")]
    pub anthropic: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Args)]
#[serde(default)]
pub struct ProviderConfig {
    #[arg(short, long, global = true)]
    pub model: Option<String>,

    #[command(flatten)]
    #[serde(alias = "base_url")]
    pub endpoint: ProviderEndpoint,

    /// API key for the provider
    #[arg(long, global = true)]
    pub api_key: Option<Secret<String>>,

    /// Sampling temperature (config file only)
    #[arg(skip)]
    pub temperature: Option<f32>,
}

impl ProviderConfig {
    pub fn merge(self, other: Self) -> Self {
        let model = other.model.or(self.model);

        let mut endpoint = self.endpoint;
        if other.endpoint.openai.is_some() || other.endpoint.anthropic.is_some() {
            endpoint.openai = other.endpoint.openai;
            endpoint.anthropic = other.endpoint.anthropic;
            endpoint.name = None; // Priority to custom
        } else if other.endpoint.name.is_some() {
            endpoint.name = other.endpoint.name;
            endpoint.openai = None;
            endpoint.anthropic = None;
        }

        let api_key = other.api_key.or(self.api_key);
        let temperature = other.temperature.or(self.temperature);

        Self {
            model,
            endpoint,
            api_key,
            temperature,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PieConfig {
    pub default_provider: Option<String>,
    #[serde(default)]
    pub provider: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub secrets: HashMap<String, Secret<String>>,
    #[serde(default)]
    pub model: HashMap<String, ModelTier>,
    #[serde(default)]
    pub mcp: HashMap<String, McpServerConfig>,
    /// The `pie server` daemon: `[server]`.
    #[serde(default)]
    pub server: ServerConfig,
    /// Per-model token pricing in USD per million tokens, keyed by the
    /// exact model id: `[pricing."claude-sonnet-4"]`. A model without an
    /// entry gets token stats but no cost.
    #[serde(default)]
    pub pricing: HashMap<String, ModelPricing>,
    pub agent: Option<GlobalAgentConfig>,
    pub sandbox: Option<SandboxConfig>,
    pub output_format: Option<String>,
    pub log_level: Option<String>,
}

/// The `pie server` daemon (`[server]` in pie.toml): the a2acp A2A
/// gateway, hosted by pie with itself as the in-process agent.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Address to bind, e.g. `127.0.0.1:8629`.
    pub bind: String,
    /// Obsolete: static bearer auth died with the pie-a2a fork. The
    /// gateway authenticates with `OpenID` Connect (`openid_connect_url`)
    /// or trusts the loopback/tailnet (expose it through `tailscale
    /// serve`). Kept parsed so a leftover key fails loudly at startup
    /// instead of silently disabling auth.
    pub api_key: Option<Secret<String>>,
    /// Hostnames remote clients will use to reach the server (e.g.
    /// `"citadel.lvh.me"`). The transport rejects any non-loopback `Host`
    /// header that is not listed here — a DNS-rebinding guard — so remote
    /// access without this list fails with 403.
    pub allowed_hosts: Vec<String>,
    /// Public URL baked into the agent card's `supportedInterfaces` (what
    /// clients dial) when the bind address is not it — e.g. the tailscale
    /// HTTPS URL when the daemon sits behind `tailscale serve`.
    pub url: Option<String>,
    /// `OpenID` Connect discovery URL. When set, the agent card declares
    /// the standard `openIdConnect` scheme and every RPC must carry a
    /// provider-issued bearer JWT. Required for non-loopback binds;
    /// unset = no auth in the application (loopback bind; tailscale is
    /// the authentication).
    pub openid_connect_url: Option<String>,
    /// Expected token audience; validated against the token's `aud` when
    /// set.
    pub audience: Option<String>,
    /// External ACP agents served alongside in-process pie
    /// (`[server.agents.<name>]` with `command`/`args`) — the server
    /// counterpart of the interactive `--acp-agent` flag.
    pub agents: BTreeMap<String, ServerAgentConfig>,
}

/// One external ACP agent behind `pie server` (`[server.agents.<name>]`):
/// an ACP-speaking command the gateway spawns per session.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerAgentConfig {
    /// Command to run, e.g. `opencode`.
    pub command: String,
    /// Arguments, e.g. `["acp"]`.
    #[serde(default)]
    pub args: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8629".to_string(),
            api_key: None,
            allowed_hosts: Vec::new(),
            url: None,
            openid_connect_url: None,
            audience: None,
            agents: BTreeMap::new(),
        }
    }
}

impl ServerConfig {
    /// Whether the bind address is a loopback interface.
    pub fn is_loopback_bind(&self) -> bool {
        use std::net::ToSocketAddrs as _;
        self.bind
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
            .is_some_and(|addr| addr.ip().is_loopback())
    }
}

/// Token pricing for a model, in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Uncached prompt (input) tokens.
    pub input: f64,
    /// Prompt tokens served from the provider's prompt cache. Defaults to
    /// `input` when unset (no discount assumed).
    #[serde(default)]
    pub cached_input: Option<f64>,
    /// Completion (output) tokens.
    pub output: f64,
}

impl ModelPricing {
    pub fn cached_input(&self) -> f64 {
        self.cached_input.unwrap_or(self.input)
    }
}

/// An HTTP-based MCP server under `[mcp.<name>]`. Its tools show up to
/// agents as `{name}__{tool}` once the agent lists `mcp` (or `mcp:<name>`)
/// in its `plugins`.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    /// Streamable-HTTP MCP endpoint (e.g. `https://mcp.example.com/mcp`).
    pub url: Url,
    /// Bearer token for servers that require one. A value that exactly
    /// matches a `[secrets]` key is replaced by that secret's value at load
    /// time, then sent as `Authorization: Bearer <value>`.
    #[serde(default)]
    pub api_key: Option<Secret<String>>,
    /// Headers sent with every request. A value that exactly matches a
    /// `[secrets]` key is replaced by that secret's value at load time.
    #[serde(default)]
    pub headers: HashMap<String, Secret<String>>,
    /// Only top-level runs (depth 0) connect this server. Nested runs —
    /// turns served by the `pie` MCP daemon itself — never see it, so an
    /// agent spawned through pie cannot spawn another through the same
    /// door.
    #[serde(default)]
    pub main_agent_only: bool,
}

impl Serialize for McpServerConfig {
    /// Header and api-key values are secrets by construction; only header
    /// names survive serialization.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("McpServerConfig", 3)?;
        s.serialize_field("url", self.url.as_str())?;
        s.serialize_field("headers", &self.headers.keys().collect::<Vec<_>>())?;
        s.serialize_field("main_agent_only", &self.main_agent_only)?;
        s.end()
    }
}

/// Header carrying the bearer token from `api_key`.
const AUTHORIZATION_HEADER: &str = "AUTHORIZATION";

impl McpServerConfig {
    /// Replace header and api-key values that exactly match a `[secrets]`
    /// key with the secret's value; literal values pass through untouched.
    pub fn resolve_secrets(&mut self, secrets: &HashMap<String, Secret<String>>) {
        if let Some(key) = &mut self.api_key
            && let Some(val) = secrets.get(key.expose_secret())
        {
            *key = val.clone();
        }
        for header in self.headers.values_mut() {
            if let Some(val) = secrets.get(header.expose_secret()) {
                *header = val.clone();
            }
        }
    }

    /// Header map for the MCP transport: the configured `headers`, plus
    /// `api_key` as `Authorization: Bearer <value>` unless a header already
    /// sets it (whatever its casing) — an explicit header wins.
    pub fn http_headers(&self) -> HashMap<String, String> {
        let mut headers: HashMap<String, String> = self
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.expose_secret().clone()))
            .collect();
        if let Some(key) = &self.api_key
            && !headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case(AUTHORIZATION_HEADER))
        {
            headers.insert(
                AUTHORIZATION_HEADER.to_string(),
                format!("Bearer {}", key.expose_secret()),
            );
        }
        headers
    }
}

/// A named model tier in `[model.<name>]` sections.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelTier {
    pub provider: String,
    pub model: Option<String>,
}

impl PieConfig {
    pub fn output_format(&self) -> OutputFormat {
        match self.output_format.as_deref() {
            Some("json") => OutputFormat::Json(None),
            Some("markdown" | "md") => OutputFormat::Markdown,
            _ => OutputFormat::Default,
        }
    }

    pub fn log_level(&self) -> &str {
        self.log_level.as_deref().unwrap_or("info")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GlobalAgentConfig {
    #[serde(default)]
    pub retry: RetryConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct RetryConfig {
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub api_error: ApiErrorConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct RateLimitConfig {
    pub max_errors: u32,
    pub retry_delay_secs: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_errors: 5,
            retry_delay_secs: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ApiErrorConfig {
    pub max_errors: u32,
    pub retry_delay_secs: u64,
}

impl Default for ApiErrorConfig {
    fn default() -> Self {
        Self {
            max_errors: 10,
            retry_delay_secs: 10,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct LaunchConfig {
    pub sandbox: Option<SandboxConfig>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub aliases: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::Figment;
    use figment::providers::{Format, Toml};

    fn parse(toml: &str) -> PieConfig {
        Figment::new().merge(Toml::string(toml)).extract().unwrap()
    }

    #[test]
    fn parse_mcp_server_config() {
        let pie = parse(
            r#"
[mcp.deepwiki]
url = "https://mcp.deepwiki.com/mcp"

[mcp.context7]
url = "https://mcp.context7.com/mcp"
[mcp.context7.headers]
CONTEXT7_API_KEY = "context7_key"
"#,
        );

        assert_eq!(pie.mcp.len(), 2);
        let deepwiki = &pie.mcp["deepwiki"];
        assert_eq!(deepwiki.url.as_str(), "https://mcp.deepwiki.com/mcp");
        assert!(deepwiki.headers.is_empty());

        let context7 = &pie.mcp["context7"];
        assert_eq!(
            context7.headers["CONTEXT7_API_KEY"].expose_secret(),
            "context7_key"
        );
    }

    #[test]
    fn parse_mcp_api_key() {
        let pie = parse(
            r#"
[mcp.mem]
url = "https://mcp.example.com/mcp"
api_key = "MEM_KEY"
"#,
        );
        assert_eq!(
            pie.mcp["mem"].api_key.as_ref().unwrap().expose_secret(),
            "MEM_KEY",
            "api_key is optional but parsed when present"
        );
    }

    #[test]
    fn mcp_section_is_optional() {
        let pie = parse("log_level = \"info\"");
        assert!(pie.mcp.is_empty());
    }

    #[test]
    fn server_section_defaults_to_loopback_bind() {
        let pie = parse("log_level = \"info\"");
        assert_eq!(pie.server.bind, "127.0.0.1:8629");
        assert!(pie.server.api_key.is_none());
        assert!(pie.server.openid_connect_url.is_none());
        assert!(pie.server.agents.is_empty());
        assert!(pie.server.is_loopback_bind());

        let pie = parse(
            r#"
[server]
bind = "0.0.0.0:8629"
"#,
        );
        assert_eq!(pie.server.bind, "0.0.0.0:8629");
        assert!(!pie.server.is_loopback_bind());
    }

    #[test]
    fn server_section_parses_auth_hosts_and_agents() {
        let pie = parse(
            r#"
[server]
allowed_hosts = ["citadel.lvh.me"]
url = "https://pie.tailnet.example"
openid_connect_url = "https://idp.example/.well-known/openid-configuration"
audience = "pie"

[server.agents.opencode]
command = "opencode"
args = ["acp"]
"#,
        );
        let server = &pie.server;
        assert_eq!(server.allowed_hosts, vec!["citadel.lvh.me".to_string()]);
        assert_eq!(server.url.as_deref(), Some("https://pie.tailnet.example"));
        assert_eq!(
            server.openid_connect_url.as_deref(),
            Some("https://idp.example/.well-known/openid-configuration")
        );
        assert_eq!(server.audience.as_deref(), Some("pie"));
        assert_eq!(
            server.agents.get("opencode"),
            Some(&ServerAgentConfig {
                command: "opencode".to_string(),
                args: vec!["acp".to_string()],
            })
        );
    }

    #[test]
    fn obsolete_server_api_key_is_still_parsed() {
        // A leftover static key must not break config loading (pie refuses
        // it loudly at server startup instead).
        let pie = parse(
            r#"
[server]
api_key = "old-key"
"#,
        );
        assert_eq!(
            pie.server
                .api_key
                .as_ref()
                .map(|k| k.expose_secret().as_str()),
            Some("old-key")
        );
    }

    #[test]
    fn parse_pricing_defaults_cached_to_input_rate() {
        let pie = parse(
            r#"
[pricing."some-model"]
input = 1.5
output = 6.0

[pricing."other-model"]
input = 1.0
cached_input = 0.1
output = 2.0
"#,
        );
        assert_eq!(pie.pricing.len(), 2);
        let some = &pie.pricing["some-model"];
        assert!((some.input - 1.5).abs() < 1e-9);
        assert!(some.cached_input.is_none());
        assert!(
            (some.cached_input() - 1.5).abs() < 1e-9,
            "unset cached_input bills at input rate"
        );
        assert!((pie.pricing["other-model"].cached_input() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn resolve_secrets_replaces_exact_matches_only() {
        let mut server = McpServerConfig {
            url: "https://mcp.example.com/mcp".parse().unwrap(),
            api_key: Some(Secret::new("mem_key".to_string())),
            headers: HashMap::from([
                ("AUTH".to_string(), Secret::new("ctx_key".to_string())),
                (
                    "X-LITERAL".to_string(),
                    Secret::new("Bearer literal".to_string()),
                ),
            ]),
            main_agent_only: false,
        };

        let mut secrets = HashMap::new();
        secrets.insert("ctx_key".to_string(), Secret::new("real-key".to_string()));
        secrets.insert("mem_key".to_string(), Secret::new("real-mem".to_string()));
        server.resolve_secrets(&secrets);

        assert_eq!(
            server.api_key.unwrap().expose_secret(),
            "real-mem",
            "api_key resolves through [secrets] too"
        );
        assert_eq!(server.headers["AUTH"].expose_secret(), "real-key");
        assert_eq!(
            server.headers["X-LITERAL"].expose_secret(),
            "Bearer literal"
        );
    }

    #[test]
    fn http_headers_composes_bearer_from_api_key() {
        let mut server = McpServerConfig {
            url: "https://mcp.example.com/mcp".parse().unwrap(),
            api_key: Some(Secret::new("real-mem".to_string())),
            headers: HashMap::new(),
            main_agent_only: false,
        };
        server.resolve_secrets(&HashMap::new());

        let headers = server.http_headers();
        assert_eq!(
            headers["AUTHORIZATION"].as_str(),
            "Bearer real-mem",
            "api_key becomes an Authorization bearer header"
        );
    }

    #[test]
    fn explicit_authorization_header_wins_over_api_key() {
        let server = McpServerConfig {
            url: "https://mcp.example.com/mcp".parse().unwrap(),
            api_key: Some(Secret::new("ignored".to_string())),
            headers: HashMap::from([(
                "authorization".to_string(),
                Secret::new("Bearer custom".to_string()),
            )]),
            main_agent_only: false,
        };

        let headers = server.http_headers();
        assert_eq!(headers.len(), 1, "no duplicate auth header is sent");
        assert_eq!(headers["authorization"], "Bearer custom");
    }

    #[test]
    fn server_without_api_key_sends_only_configured_headers() {
        let server = McpServerConfig {
            url: "https://mcp.example.com/mcp".parse().unwrap(),
            api_key: None,
            headers: HashMap::from([(
                "CONTEXT7_API_KEY".to_string(),
                Secret::new("ctx".to_string()),
            )]),
            main_agent_only: false,
        };

        let headers = server.http_headers();
        assert_eq!(headers["CONTEXT7_API_KEY"], "ctx");
        assert_eq!(headers.len(), 1);
    }
}
