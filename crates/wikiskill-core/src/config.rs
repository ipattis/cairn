//! Configuration for the daemon and the loop.
//!
//! Model IDs are deliberately strings with no defaults baked into code paths that
//! spend money: the rationale says to confirm exact IDs in the Fireworks catalog and
//! the Bedrock console, and to re-baseline whenever an ID's backing model changes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

/// Which endpoint a role's model is called through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Endpoint {
    /// `bedrock-runtime`: the default for new applications; cross-Region inference
    /// and Guardrails live only here.
    BedrockRuntime,
    /// `bedrock-mantle`: Mantle-only models, background inference, Projects.
    /// No Guardrails support.
    BedrockMantle,
    /// Fireworks AI, OpenAI-compatible.
    Fireworks,
}

/// A model binding for one role.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRef {
    /// Provider-qualified model ID, e.g. `fireworks-ai/<deepseek-v4-pro-id>`.
    pub id: String,
    pub endpoint: Endpoint,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub temperature: Option<f32>,
}

fn default_max_tokens() -> u32 {
    8192
}

/// Per-role model assignments (see the provider and model plan).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Models {
    /// Rollouts, gate and daily coding. Skills are evolved and gated on this one.
    pub inference: ModelRef,
    /// Optional cheap exploratory rollout model; candidates only, never defaults.
    #[serde(default)]
    pub exploratory: Option<ModelRef>,
    pub maintainer: ModelRef,
    pub proposer: ModelRef,
}

/// Jev decision-layer settings. Phases 0-2 run with `enabled = false`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JevConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Log answers without acting on them. Every integration ships this way first.
    #[serde(default = "default_true")]
    pub shadow_mode: bool,
    #[serde(default = "default_jev_endpoint")]
    pub endpoint: String,
    /// Pinned version, recorded in each note's `labeled_by` field.
    #[serde(default = "default_jev_version")]
    pub version: String,
    /// Below this confidence a label goes to the cockpit's Labels view instead.
    #[serde(default = "default_confidence_floor")]
    pub confidence_floor: f32,
    /// State plus the longest question must fit in ~32k tokens.
    #[serde(default = "default_jev_state_budget")]
    pub state_token_budget: usize,
}

fn default_true() -> bool {
    true
}
fn default_jev_endpoint() -> String {
    "https://api.typesafe.sh/v1/systemone".to_string()
}
fn default_jev_version() -> String {
    "jev-1.13.0".to_string()
}
fn default_confidence_floor() -> f32 {
    0.75
}
fn default_jev_state_budget() -> usize {
    28_000
}

impl Default for JevConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            shadow_mode: true,
            endpoint: default_jev_endpoint(),
            version: default_jev_version(),
            confidence_floor: default_confidence_floor(),
            state_token_budget: default_jev_state_budget(),
        }
    }
}

/// How the loop reaches OpenCode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorConfig {
    /// Base URL of the daemon-supervised `opencode serve`.
    #[serde(default = "default_opencode_url")]
    pub base_url: String,
    /// Path to the `opencode` binary the daemon supervises.
    #[serde(default = "default_opencode_bin")]
    pub binary: String,
    /// Pinned V1 release line. A mismatch is a hard error, not a warning: V2 revises
    /// the server API.
    #[serde(default = "default_opencode_version")]
    pub pinned_version: String,
    /// Name of the rollout agent file the loop regenerates each iteration.
    #[serde(default = "default_agent_name")]
    pub agent: String,
    /// Isolated `HOME`/`XDG_CONFIG_HOME` for the rollout server, so it never loads
    /// the daily skills symlink or MCP servers.
    pub rollout_home: PathBuf,
    /// Cap on rollouts in flight against one server.
    #[serde(default = "default_parallel")]
    pub max_parallel_rollouts: usize,
}

fn default_opencode_url() -> String {
    "http://127.0.0.1:4096".to_string()
}
fn default_opencode_bin() -> String {
    "opencode".to_string()
}
fn default_opencode_version() -> String {
    "1.18".to_string()
}
fn default_agent_name() -> String {
    "wiki-inference".to_string()
}
fn default_parallel() -> usize {
    4
}

/// Gate rule. The paper's rule is [`GateRule::Strict`]; the mean-of-two variant is an
/// explicit deviation and is recorded in every impact entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateRule {
    /// Accept only if a single validation run strictly beats the best so far.
    Strict,
    /// Run validation twice and gate on the mean, still strictly.
    MeanOfTwo,
}

impl GateRule {
    /// Text written into `skill-impact.md` so a reader knows which rule produced it.
    pub fn label(self) -> &'static str {
        match self {
            GateRule::Strict => "strict, single run",
            GateRule::MeanOfTwo => "strict, mean of two runs",
        }
    }

    pub fn validation_runs(self) -> usize {
        match self {
            GateRule::Strict => 1,
            GateRule::MeanOfTwo => 2,
        }
    }
}

/// Loop budget and gating knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopConfig {
    /// The paper's accepted updates spread across iterations 0-7.
    #[serde(default = "default_iterations")]
    pub iterations: u32,
    #[serde(default = "default_gate_rule")]
    pub gate_rule: GateRule,
    /// Stop early once validation reaches 100%.
    #[serde(default = "default_true")]
    pub stop_at_perfect: bool,
    /// Turn cap for the in-process curator agent loops.
    #[serde(default = "default_turn_cap")]
    pub curator_turn_cap: u32,
    /// Seconds a single rollout may run before it is abandoned and scored 0.
    #[serde(default = "default_rollout_timeout")]
    pub rollout_timeout_secs: u64,
}

fn default_iterations() -> u32 {
    8
}
fn default_gate_rule() -> GateRule {
    GateRule::Strict
}
fn default_turn_cap() -> u32 {
    24
}
fn default_rollout_timeout() -> u64 {
    900
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            iterations: default_iterations(),
            gate_rule: default_gate_rule(),
            stop_at_perfect: true,
            curator_turn_cap: default_turn_cap(),
            rollout_timeout_secs: default_rollout_timeout(),
        }
    }
}

/// Localhost API surface of the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Bearer token for the localhost API. Generated on first run and stored in the
    /// support directory with 0600 permissions.
    #[serde(default)]
    pub token: Option<String>,
    /// `~/Library/Application Support/wikiskill` unless overridden.
    pub state_dir: PathBuf,
}

fn default_bind() -> String {
    "127.0.0.1:8787".to_string()
}

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Obsidian vault that is also a git repo, kept outside Documents, Desktop and
    /// iCloud Drive.
    pub vault: PathBuf,
    pub models: Models,
    pub executor: ExecutorConfig,
    #[serde(default)]
    pub jev: JevConfig,
    #[serde(default)]
    pub r#loop: LoopConfig,
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub sandbox: sandbox::ProfileConfig,
    #[serde(default)]
    pub verifier: verifier::VerifierConfig,
}

use crate::{sandbox, verifier};

impl Config {
    pub async fn load(path: &Path) -> Result<Self> {
        let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
        let cfg: Config = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing config {}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub async fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, serde_json::to_vec_pretty(self)?).await?;
        Ok(())
    }

    /// Checks the constraints the rationale states as hard rules.
    pub fn validate(&self) -> Result<()> {
        if !self.vault.is_absolute() {
            anyhow::bail!("vault path must be absolute: {}", self.vault.display());
        }
        for forbidden in ["/Documents/", "/Desktop/", "/Library/Mobile Documents/"] {
            if self.vault.to_string_lossy().contains(forbidden) {
                anyhow::bail!(
                    "vault must live outside Documents, Desktop and iCloud Drive \
                     (macOS privacy prompts would block the daemon): {}",
                    self.vault.display()
                );
            }
        }
        if self.r#loop.iterations == 0 {
            anyhow::bail!("loop.iterations must be at least 1");
        }
        if self.executor.max_parallel_rollouts == 0 {
            anyhow::bail!("executor.max_parallel_rollouts must be at least 1");
        }
        if self.models.maintainer.endpoint == Endpoint::BedrockMantle
            || self.models.proposer.endpoint == Endpoint::BedrockMantle
        {
            // Not fatal, but worth refusing by default: Mantle has no Guardrails.
            tracing::warn!(
                "a curator role is pinned to bedrock-mantle, which does not support \
                 Bedrock Guardrails"
            );
        }
        Ok(())
    }

    /// A config skeleton with the plan's role assignments and placeholder model IDs.
    pub fn sample(vault: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            vault,
            models: Models {
                inference: ModelRef {
                    id: "fireworks-ai/DEEPSEEK-V4-PRO-ID".into(),
                    endpoint: Endpoint::Fireworks,
                    max_tokens: 8192,
                    temperature: Some(1.0),
                },
                exploratory: Some(ModelRef {
                    id: "fireworks-ai/DEEPSEEK-V4-FLASH-ID".into(),
                    endpoint: Endpoint::Fireworks,
                    max_tokens: 8192,
                    temperature: Some(1.0),
                }),
                maintainer: ModelRef {
                    id: "claude-sonnet-5".into(),
                    endpoint: Endpoint::BedrockRuntime,
                    max_tokens: 16384,
                    temperature: Some(0.2),
                },
                proposer: ModelRef {
                    id: "claude-opus-5".into(),
                    endpoint: Endpoint::BedrockRuntime,
                    max_tokens: 16384,
                    temperature: Some(0.3),
                },
            },
            executor: ExecutorConfig {
                base_url: default_opencode_url(),
                binary: default_opencode_bin(),
                pinned_version: default_opencode_version(),
                agent: default_agent_name(),
                rollout_home: state_dir.join("rollout-home"),
                max_parallel_rollouts: default_parallel(),
            },
            jev: JevConfig::default(),
            r#loop: LoopConfig::default(),
            daemon: DaemonConfig {
                bind: default_bind(),
                token: None,
                state_dir,
            },
            sandbox: sandbox::ProfileConfig::default(),
            verifier: verifier::VerifierConfig::default(),
        }
    }
}

/// Default support directory: `~/Library/Application Support/wikiskill`.
pub fn default_state_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join("Library/Application Support/wikiskill")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_vault_in_icloud() {
        let cfg = Config::sample(
            PathBuf::from("/Users/x/Library/Mobile Documents/Vaults/WikiSkill"),
            PathBuf::from("/tmp/state"),
        );
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn accepts_vault_outside_synced_dirs() {
        let cfg = Config::sample(
            PathBuf::from("/Users/x/Vaults/Agents/WikiSkill"),
            PathBuf::from("/tmp/state"),
        );
        cfg.validate().unwrap();
    }

    #[test]
    fn roundtrips_through_json() {
        let cfg = Config::sample(
            PathBuf::from("/Users/x/Vaults/Agents/WikiSkill"),
            PathBuf::from("/tmp/state"),
        );
        let text = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.models.inference.id, cfg.models.inference.id);
        assert_eq!(back.r#loop.gate_rule, GateRule::Strict);
    }

    #[test]
    fn gate_rule_labels_are_recorded_verbatim() {
        assert_eq!(GateRule::Strict.label(), "strict, single run");
        assert_eq!(GateRule::MeanOfTwo.validation_runs(), 2);
    }
}
