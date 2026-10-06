use adapter_protocol::{AdapterError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const DEFAULT_MODEL_PRIORITY: &[&str] = &["gpt-6-astra", "gpt-5.6-sol"];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Mai,
    Github,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AccountScope {
    pub provider: Provider,
    pub account: String,
}

impl AccountScope {
    pub fn new(provider: Provider, account: impl Into<String>) -> Result<Self> {
        let account = account.into();
        if account.trim().is_empty() {
            return Err(AdapterError::invalid(
                "An explicit account/upstream scope is required.",
            ));
        }
        let account = if provider == Provider::Github {
            account.to_ascii_lowercase()
        } else {
            account
        };
        Ok(Self { provider, account })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Capability<T> {
    Unknown,
    Unsupported,
    Supported(T),
}

#[derive(Clone, Debug)]
pub struct AvailableProvider {
    pub scope: AccountScope,
    pub models: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedRoute {
    pub scope: AccountScope,
    pub model: String,
}

pub fn validate_priority(models: &[String]) -> Result<()> {
    let mut seen = HashSet::new();
    if models.is_empty()
        || models
            .iter()
            .any(|model| model.is_empty() || model.trim() != model || !seen.insert(model.as_str()))
    {
        return Err(AdapterError::invalid(
            "Model priority must be a nonempty ordered list of unique, nonempty IDs.",
        ));
    }
    Ok(())
}

pub fn select(available: &[AvailableProvider], priority: &[String]) -> Result<SelectedRoute> {
    validate_priority(priority)?;
    let mut providers = HashSet::new();
    for provider in available {
        if !providers.insert(provider.scope.provider) {
            return Err(AdapterError::invalid(
                "Only one configured account per provider is allowed.",
            ));
        }
        let mut models = HashSet::new();
        if provider
            .models
            .iter()
            .any(|model| model.trim().is_empty() || !models.insert(model))
        {
            return Err(AdapterError::upstream(
                "The provider returned invalid or duplicate model IDs.",
            ));
        }
    }
    for model in priority {
        for name in [Provider::Mai, Provider::Github] {
            if let Some(provider) = available
                .iter()
                .find(|provider| provider.scope.provider == name && provider.models.contains(model))
            {
                return Ok(SelectedRoute {
                    scope: provider.scope.clone(),
                    model: model.clone(),
                });
            }
        }
    }
    Err(AdapterError::new(
        503,
        "no_preferred_model",
        "No configured preferred model is available. No unranked model or alternate account was selected.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(provider: Provider, models: &[&str]) -> AvailableProvider {
        AvailableProvider {
            scope: AccountScope::new(provider, "synthetic-account").unwrap(),
            models: models.iter().map(|model| model.to_string()).collect(),
        }
    }

    fn priority() -> Vec<String> {
        DEFAULT_MODEL_PRIORITY
            .iter()
            .map(|model| model.to_string())
            .collect()
    }

    #[test]
    fn model_priority_precedes_the_mai_tie_break() {
        let sources = [
            source(Provider::Mai, &["gpt-5.6-sol"]),
            source(Provider::Github, &["gpt-6-astra"]),
        ];
        let selected = select(&sources, &priority()).unwrap();
        assert_eq!(selected.scope.provider, Provider::Github);
        assert_eq!(selected.model, "gpt-6-astra");
    }

    #[test]
    fn mai_wins_a_same_model_tie_independently_of_discovery_order() {
        let sources = [
            source(Provider::Github, &["gpt-6-astra"]),
            source(Provider::Mai, &["gpt-6-astra"]),
        ];
        assert_eq!(
            select(&sources, &priority()).unwrap().scope.provider,
            Provider::Mai
        );
    }

    #[test]
    fn unknown_models_are_not_implicitly_ranked() {
        let sources = [source(Provider::Github, &["future-model"])];
        assert_eq!(select(&sources, &priority()).unwrap_err().status, 503);
        assert_eq!(
            select(&sources, &["future-model".into()]).unwrap().model,
            "future-model"
        );
    }

    #[test]
    fn priorities_and_duplicate_provider_accounts_are_rejected() {
        for models in [vec![], vec!["".into()], vec!["same".into(), "same".into()]] {
            assert!(validate_priority(&models).is_err());
        }
        assert!(
            select(
                &[
                    source(Provider::Github, &["gpt-6-astra"]),
                    source(Provider::Github, &["gpt-6-astra"])
                ],
                &priority(),
            )
            .is_err()
        );
    }
}
