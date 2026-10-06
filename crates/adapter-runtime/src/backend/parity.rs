//! Narrow compatibility at provider boundaries, without a generic proxy fallback.
use super::*;
use reqwest::Method;

fn remote_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 256
        || id.chars().any(char::is_control)
        || id.contains('\\')
        || id.split('/').any(|part| matches!(part, "." | ".."))
    {
        return Err(AdapterError::invalid(
            "A remote MAI response ID must be 1 to 256 bytes without controls, backslashes or dot segments.",
        ));
    }
    Ok(())
}

pub(super) fn published_id(resource: &Value) -> Result<()> {
    remote_id(resource["id"].as_str().unwrap_or(""))
        .map_err(|error| AdapterError::upstream(error.message))
}

impl Backend {
    /// Explicit override for the reserved Codex review model; never another account.
    pub fn set_review_model(&self, model: Option<String>) -> Result<()> {
        if model.as_ref().is_some_and(|m| {
            m.is_empty()
                || m.len() > 200
                || m == "codex-auto-review"
                || m.chars().any(char::is_whitespace)
                || m.chars().any(char::is_control)
        }) {
            return Err(AdapterError::invalid(
                "The review model must be an explicit upstream model ID.",
            ));
        }
        *self
            .review_model
            .write()
            .map_err(|_| AdapterError::upstream("Review settings are unavailable."))? = model;
        Ok(())
    }

    pub(super) fn review_target(&self, catalog: &Catalog, github: bool) -> Result<String> {
        let configured = self
            .review_model
            .read()
            .map_err(|_| AdapterError::upstream("Review settings are unavailable."))?
            .clone();
        let eligible = |id: &str| {
            catalog
                .models
                .iter()
                .any(|m| m.id == id && (!github || m.native()))
        };
        if let Some(model) = configured {
            return if eligible(&model) {
                Ok(model)
            } else {
                Err(AdapterError::invalid(
                    "The configured Auto-review model is not available with Responses support in this account.",
                ))
            };
        }
        std::iter::once("gpt-5.5")
            .chain(crate::selection::DEFAULT_MODEL_PRIORITY.iter().copied())
            .find(|id| eligible(id)).map(str::to_owned)
            .ok_or_else(|| AdapterError::invalid("No ranked Auto-review Responses model is available. Set GITHUB_ADAPTER_REVIEW_MODEL explicitly."))
    }

    /// MAI remote IDs stay at the one selected endpoint. GitHub IDs remain local.
    pub async fn response_history(
        &self,
        id: &str,
        delete: bool,
        ctx: RequestContext,
    ) -> Result<Value> {
        let _guard = ctx.cancel_on_drop();
        if self.scope.provider == Provider::Github {
            if !delete {
                return Ok(self.history.read(id)?.response.clone());
            }
            if !self.history.discard(id)? {
                return Err(AdapterError::new(
                    404,
                    "not_found_error",
                    "Response history is unavailable.",
                ));
            }
            return Ok(json!({"id": id, "object": "response.deleted", "deleted": true}));
        }
        if !delete && let Ok(saved) = self.history.read(id) {
            return Ok(saved.response.clone());
        }
        remote_id(id)?;
        let binding = self.binding(&ctx).await?;
        let mut url = transport::append_path(binding.endpoint(), "/v1/responses")?;
        url.path_segments_mut()
            .map_err(|_| AdapterError::invalid("MAI requires a hierarchical endpoint."))?
            .push(id);
        let request = binding
            .client()
            .request(if delete { Method::DELETE } else { Method::GET }, url)
            .header(ACCEPT, "application/json")
            .header(ACCEPT_ENCODING, "identity")
            .timeout(transport::timeout(&ctx, Some(transport::CONTROL_TIMEOUT))?);
        let response = transport::send(request, &ctx, "MAI response history").await?;
        let status = response.status().as_u16();
        let value = transport::read_json(
            response,
            &ctx,
            transport::INFERENCE_BYTES,
            "MAI response history",
        )
        .await?;
        if !(200..300).contains(&status) {
            return Err(AdapterError::new(
                transport::error_status(status),
                "upstream_error",
                transport::message(&value, "MAI response retrieval failed."),
            ));
        }
        if value["id"] != id || (delete && value["deleted"] != true) {
            return Err(AdapterError::upstream(
                "MAI returned an invalid or mismatched response resource.",
            ));
        }
        let value = if delete {
            value
        } else {
            resource_value(value, ResourceUse::Retrieval)?
        };
        if delete {
            self.history.discard(id)?;
        }
        Ok(value)
    }
}
