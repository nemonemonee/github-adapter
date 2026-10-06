use super::Backend;
use crate::context::RequestContext;
use crate::selection::Provider;
use adapter_protocol::{AdapterError, Result, Value, anthropic};
use serde_json::json;

impl Backend {
    pub async fn complete_messages(
        &self,
        payload: Value,
        context: RequestContext,
    ) -> Result<Value> {
        let _guard = context.cancel_on_drop();
        let requested = payload
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| AdapterError::invalid("A model ID is required."))?;
        let extras = anthropic::validate_request(&payload)?;
        let mai = self.scope.provider == Provider::Mai;
        if mai && requested.starts_with("claude-") {
            return Err(AdapterError::invalid(
                "Use an explicit supported MAI model alias.",
            ));
        }
        let cap = (mai && requested == "gpt-5.6-sol").then_some(128_000);
        let mut request = anthropic::to_responses(&payload, requested, None, cap)?;
        request
            .as_object_mut()
            .ok_or_else(|| AdapterError::invalid("Invalid converted request."))?
            .extend(
                extras
                    .as_object()
                    .ok_or_else(|| AdapterError::invalid("Invalid Anthropic controls."))?
                    .clone(),
            );
        request["store"] = json!(false);
        let response = self.complete_responses(request, context).await?;
        anthropic::to_message(&anthropic::validate_response(&response)?, requested)
    }
}
