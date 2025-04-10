//! Client for OpenAI-compatible APIs (usually LiteLLM or Ollama).

use async_openai::{Client, config::OpenAIConfig};

use crate::prelude::*;

/// Create an OpenAI-compatible client using the default configuration.
pub fn create_llm_client() -> Result<Client<OpenAIConfig>> {
    let mut client_config = OpenAIConfig::new();
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        client_config = client_config.with_api_key(api_key);
    }
    if let Ok(api_base) = std::env::var("OPENAI_API_BASE") {
        client_config = client_config.with_api_base(api_base);
    }
    let client = Client::with_config(client_config);
    Ok(client)
}
