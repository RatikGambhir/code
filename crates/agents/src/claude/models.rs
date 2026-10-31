//! Models and accounts for the Claude backend.

use serde_json::Value;

use crate::protocol::{Account, GetAccountResponse, Model, ReasoningEffortOption};

/// The model new Claude threads use unless the user picks another.
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";

const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The current Claude models, newest first. Claude Code's own list (from
/// its `initialize` response) only knows the models of its release, so
/// these lead and its extra entries follow.
pub fn current_models() -> Vec<Model> {
    [
        (DEFAULT_MODEL, "Claude Opus 5.5", "Most capable Opus for coding and agents", "medium"),
        ("claude-sonnet-5-5", "Claude Sonnet 5.5", "Fast and capable for everyday coding", "high"),
        ("claude-haiku-5-5", "Claude Haiku 5.5", "Fastest, for quick answers and small edits", "medium"),
        ("claude-fable-5-1", "Claude Fable 5.1", "Most capable, for the hardest and longest tasks", "high"),
    ]
    .into_iter()
    .map(|(id, name, description, default_effort)| Model {
        id: id.into(),
        model: id.into(),
        display_name: name.into(),
        description: description.into(),
        hidden: false,
        supported_reasoning_efforts: efforts(EFFORTS.iter().copied()),
        default_reasoning_effort: default_effort.into(),
        input_modalities: vec!["text".into(), "image".into()],
        is_default: id == DEFAULT_MODEL,
    })
    .collect()
}

fn efforts<'a>(levels: impl Iterator<Item = &'a str>) -> Vec<ReasoningEffortOption> {
    levels
        .map(|level| ReasoningEffortOption { reasoning_effort: level.into(), description: String::new() })
        .collect()
}

/// [`current_models`] followed by whatever else Claude Code offers (its
/// aliases such as `opus`, or models newer than this build). `models` is
/// the `models` array from Claude Code's `initialize` response.
pub fn merge_models(models: Option<&Value>) -> Vec<Model> {
    let mut list = current_models();
    for entry in models.and_then(Value::as_array).into_iter().flatten() {
        let Some(value) = entry["value"].as_str().filter(|v| !v.is_empty() && *v != "default") else {
            continue;
        };
        let base = value.split('[').next().unwrap_or(value);
        if list.iter().any(|m| m.id == value || m.id == base) {
            continue;
        }
        let description = entry["description"].as_str().unwrap_or_default();
        // "Opus 4.7 · Most capable…": the first part names the model.
        let display_name = description
            .split(" · ")
            .next()
            .filter(|d| !d.is_empty() && *d != "Custom model")
            .or_else(|| entry["displayName"].as_str())
            .unwrap_or(value);
        let levels: Vec<&str> = entry["supportedEffortLevels"]
            .as_array()
            .map(|l| l.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        list.push(Model {
            id: value.into(),
            model: value.into(),
            display_name: display_name.into(),
            description: description.into(),
            hidden: false,
            supported_reasoning_efforts: efforts(levels.into_iter()),
            default_reasoning_effort: String::new(),
            input_modalities: Vec::new(),
            is_default: false,
        });
    }
    list
}

/// Reads `claude auth status --json`. `auth_failed` is set when a turn was
/// refused with a 401 although the status says signed in (an expired or
/// revoked token), so the panel asks for a new sign-in.
pub fn account_from_status(status: &Value, auth_failed: bool) -> GetAccountResponse {
    let logged_in = status["loggedIn"].as_bool().unwrap_or(false);
    let provider = status["apiProvider"].as_str().unwrap_or("firstParty");
    // Bedrock, Vertex and Foundry use their own credentials.
    let signed_in = (logged_in || provider != "firstParty") && !auth_failed;
    GetAccountResponse {
        account: signed_in.then(|| Account::Claude {
            email: status["email"].as_str().map(str::to_string),
            method: status["authMethod"].as_str().map(str::to_string).or_else(|| {
                (provider != "firstParty").then(|| provider.to_string())
            }),
            plan: status["subscriptionType"].as_str().map(str::to_string),
        }),
        requires_openai_auth: !signed_in,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn current_models_lead_and_claude_code_extras_follow() {
        // Shape captured from Claude Code 2.1.131's initialize response.
        let cli = json!([
            {"value": "default", "displayName": "Default (recommended)", "description": "Sonnet 4.6 · Best for everyday tasks",
             "supportsEffort": true, "supportedEffortLevels": ["low", "medium", "high", "max"]},
            {"value": "opus", "displayName": "Opus", "description": "Opus 4.7 · Most capable for complex work",
             "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]},
            {"value": "claude-fable-5-1[1m]", "displayName": "Fable", "description": "Fable 5.1 · Most capable"},
            {"value": "haiku", "displayName": "Haiku", "description": "Haiku 4.5 · Fastest for quick answers"}
        ]);
        let models = merge_models(Some(&cli));
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-1", "opus", "haiku"]);
        assert!(models[0].is_default);
        assert_eq!(models.iter().filter(|m| m.is_default).count(), 1);
        assert_eq!(models[0].default_reasoning_effort, "medium");
        assert_eq!(models[4].display_name, "Opus 4.7");
        assert_eq!(models[4].supported_reasoning_efforts.len(), 5);
        assert!(models[5].supported_reasoning_efforts.is_empty());
        assert_eq!(merge_models(None).len(), 4);
    }

    #[test]
    fn account_status() {
        let status = json!({"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty",
            "email": "dev@example.com", "subscriptionType": "pro"});
        let account = account_from_status(&status, false);
        assert!(!account.requires_openai_auth);
        assert!(matches!(account.account, Some(Account::Claude { plan: Some(ref p), .. }) if p == "pro"));

        let expired = account_from_status(&status, true);
        assert!(expired.requires_openai_auth && expired.account.is_none());

        let signed_out = account_from_status(&json!({"loggedIn": false, "authMethod": "none", "apiProvider": "firstParty"}), false);
        assert!(signed_out.requires_openai_auth);

        let bedrock = account_from_status(&json!({"loggedIn": false, "apiProvider": "bedrock"}), false);
        assert!(matches!(bedrock.account, Some(Account::Claude { method: Some(ref m), .. }) if m == "bedrock"));
    }
}
