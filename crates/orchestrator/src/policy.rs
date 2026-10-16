//! What the IDE allows before a prompt ever reaches Codex.
//!
//! These checks are *in addition to* Codex's own sandbox and approval policy,
//! never a replacement: the IDE can only be stricter than Codex.

use std::sync::OnceLock;

use cockpit_agents::protocol::{AskForApproval, SandboxMode};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{approvals::ApprovalRules, browser_tools::BrowserToolPolicy};

/// The permission presets offered in the agent panel.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionPreset {
    /// Codex may read files and run read-only commands; it asks for the rest.
    ReadOnly,
    /// Codex may edit the workspace and run commands in its sandbox; it asks
    /// before anything outside that (network, other directories).
    #[default]
    Auto,
    /// No sandbox, no approvals. For throwaway environments.
    FullAccess,
}

impl PermissionPreset {
    pub const ALL: [PermissionPreset; 3] = [
        PermissionPreset::ReadOnly,
        PermissionPreset::Auto,
        PermissionPreset::FullAccess,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PermissionPreset::ReadOnly => "Read only",
            PermissionPreset::Auto => "Auto",
            PermissionPreset::FullAccess => "Full access",
        }
    }

    pub fn sandbox(self) -> SandboxMode {
        match self {
            PermissionPreset::ReadOnly => SandboxMode::ReadOnly,
            PermissionPreset::Auto => SandboxMode::WorkspaceWrite,
            PermissionPreset::FullAccess => SandboxMode::DangerFullAccess,
        }
    }

    pub fn approval_policy(self) -> AskForApproval {
        match self {
            PermissionPreset::ReadOnly | PermissionPreset::Auto => AskForApproval::OnRequest,
            PermissionPreset::FullAccess => AskForApproval::Never,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentPolicy {
    /// Prompts longer than this are refused.
    pub max_prompt_chars: usize,
    /// Refuse prompts that contain credentials (API keys, private keys…).
    pub block_secrets: bool,
    /// Regexes that refuse a prompt outright.
    pub blocked_patterns: Vec<String>,
    /// Regexes that only warn.
    pub warn_patterns: Vec<String>,
    pub permissions: PermissionPreset,
    /// Turns that may run at once across all threads.
    pub max_concurrent_turns: usize,
    pub approvals: ApprovalRules,
    pub browser: BrowserToolPolicy,
}

impl Default for AgentPolicy {
    fn default() -> Self {
        Self {
            max_prompt_chars: 200_000,
            block_secrets: true,
            blocked_patterns: Vec::new(),
            warn_patterns: Vec::new(),
            permissions: PermissionPreset::Auto,
            max_concurrent_turns: 3,
            approvals: ApprovalRules::default(),
            browser: BrowserToolPolicy::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptCheck {
    Allow,
    Warn(Vec<String>),
    Deny(String),
}

impl AgentPolicy {
    /// Checks a prompt before it is sent.
    pub fn check_prompt(&self, prompt: &str) -> PromptCheck {
        if prompt.trim().is_empty() {
            return PromptCheck::Deny("The prompt is empty.".into());
        }
        let length = prompt.chars().count();
        if length > self.max_prompt_chars {
            return PromptCheck::Deny(format!(
                "The prompt is {length} characters; the limit is {}.",
                self.max_prompt_chars
            ));
        }
        if self.block_secrets {
            let found = find_secrets(prompt);
            if !found.is_empty() {
                return PromptCheck::Deny(format!(
                    "The prompt appears to contain a secret ({}). Remove it before sending.",
                    found.join(", ")
                ));
            }
        }
        for pattern in &self.blocked_patterns {
            match Regex::new(pattern) {
                Ok(re) if re.is_match(prompt) => {
                    return PromptCheck::Deny(format!("The prompt matches the blocked pattern `{pattern}`."))
                }
                Ok(_) => {}
                Err(err) => tracing::warn!("ignoring invalid blocked pattern {pattern:?}: {err}"),
            }
        }
        let warnings: Vec<String> = self
            .warn_patterns
            .iter()
            .filter(|pattern| Regex::new(pattern).is_ok_and(|re| re.is_match(prompt)))
            .map(|pattern| format!("The prompt matches the pattern `{pattern}`."))
            .collect();
        if warnings.is_empty() {
            PromptCheck::Allow
        } else {
            PromptCheck::Warn(warnings)
        }
    }

    /// Patterns that fail to compile, for validating a policy before saving.
    pub fn invalid_patterns(&self) -> Vec<(String, String)> {
        self.blocked_patterns
            .iter()
            .chain(&self.warn_patterns)
            .chain(&self.approvals.auto_decline_commands)
            .filter_map(|pattern| Regex::new(pattern).err().map(|err| (pattern.clone(), err.to_string())))
            .collect()
    }
}

/// High-confidence credential formats. Deliberately narrow: a false positive
/// blocks the user, so only formats with distinctive prefixes are listed.
fn secret_patterns() -> &'static [(&'static str, Regex)] {
    static PATTERNS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            ("private key", r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |PGP |ENCRYPTED )?PRIVATE KEY-----"),
            ("AWS access key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
            ("OpenAI API key", r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_-]{32,}"),
            ("Anthropic API key", r"\bsk-ant-[A-Za-z0-9_-]{32,}"),
            ("GitHub token", r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,})"),
            ("Slack token", r"\bxox[abposr]-[A-Za-z0-9-]{10,}"),
            ("Google API key", r"\bAIza[0-9A-Za-z_-]{35}\b"),
            ("Stripe secret key", r"\b[rs]k_live_[0-9A-Za-z]{20,}"),
        ]
        .into_iter()
        .map(|(name, pattern)| (name, Regex::new(pattern).expect("valid secret pattern")))
        .collect()
    })
}

/// The kinds of secret found in `text` (never the secrets themselves).
pub fn find_secrets(text: &str) -> Vec<&'static str> {
    secret_patterns()
        .iter()
        .filter(|(_, re)| re.is_match(text))
        .map(|(name, _)| *name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_prompts_pass() {
        let policy = AgentPolicy::default();
        assert_eq!(policy.check_prompt("Refactor the login page to use the new API"), PromptCheck::Allow);
    }

    #[test]
    fn empty_and_oversized_prompts_are_refused() {
        let policy = AgentPolicy { max_prompt_chars: 10, ..Default::default() };
        assert!(matches!(policy.check_prompt("   "), PromptCheck::Deny(_)));
        assert!(matches!(policy.check_prompt("this is far too long"), PromptCheck::Deny(_)));
    }

    #[test]
    fn secrets_are_detected_without_echoing_them() {
        let key = format!("sk-proj-{}", "a1B2".repeat(12));
        let PromptCheck::Deny(message) = AgentPolicy::default().check_prompt(&format!("use {key} please")) else {
            panic!("expected a denial");
        };
        assert!(message.contains("OpenAI API key"));
        assert!(!message.contains(&key));

        assert_eq!(find_secrets("AKIAIOSFODNN7EXAMPLE is my key"), vec!["AWS access key"]);
        assert_eq!(find_secrets("-----BEGIN OPENSSH PRIVATE KEY-----"), vec!["private key"]);
        assert!(find_secrets("the word sk-learn and AKIA alone").is_empty());

        let lenient = AgentPolicy { block_secrets: false, ..Default::default() };
        assert_eq!(lenient.check_prompt(&key), PromptCheck::Allow);
    }

    #[test]
    fn custom_patterns_block_or_warn() {
        let policy = AgentPolicy {
            blocked_patterns: vec![r"(?i)production\s+database".into(), "[invalid".into()],
            warn_patterns: vec![r"(?i)\bdeploy\b".into()],
            ..Default::default()
        };
        assert!(matches!(policy.check_prompt("drop the Production  Database"), PromptCheck::Deny(_)));
        assert!(matches!(policy.check_prompt("deploy it"), PromptCheck::Warn(w) if w.len() == 1));
        assert_eq!(policy.invalid_patterns().len(), 1);
    }

    #[test]
    fn presets_map_to_codex_settings() {
        assert_eq!(PermissionPreset::Auto.sandbox(), SandboxMode::WorkspaceWrite);
        assert_eq!(PermissionPreset::Auto.approval_policy(), AskForApproval::OnRequest);
        assert_eq!(PermissionPreset::FullAccess.approval_policy(), AskForApproval::Never);
        let json = serde_json::to_string(&AgentPolicy::default()).unwrap();
        let back: AgentPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, AgentPolicy::default());
        // Stored policies from older builds fill in new fields with defaults.
        let partial: AgentPolicy = serde_json::from_str(r#"{"max_prompt_chars": 5}"#).unwrap();
        assert_eq!(partial.max_prompt_chars, 5);
        assert!(partial.block_secrets);
    }
}
