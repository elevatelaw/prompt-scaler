//! Client for OpenAI-compatible APIs (usually LiteLLM or Ollama).

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use anyhow::anyhow;
use async_openai::config::Config as _;
use serde_json::Map;
use tokio::sync::OnceCell;

use crate::{drivers::openai::get_openai_client_config, prelude::*};

/// Information about a LiteLLM model.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LiteLlmModel {
    /// The model name.
    pub model_name: String,

    /// LiteLLM parameters.
    pub litellm_params: LiteLlmModelParams,

    /// Model information.
    pub model_info: LiteLlmModelInfo,

    /// Other parameters.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

impl fmt::Display for LiteLlmModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", serde_json::to_string(self).unwrap())
    }
}

/// Parameters for a LiteLLM model.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LiteLlmModelParams {
    /// Actual model name, in the format `<provider>/<model>`.
    pub model: String,

    /// Other parameters.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// Detailed information about a LiteLLM model.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LiteLlmModelInfo {
    /// The model ID.
    pub id: String,

    /// Cost of input tokens. May be 0 for local models.
    #[serde(default)]
    pub input_cost_per_token: f64,

    /// Cost of output tokens. May be 0 for local models.
    #[serde(default)]
    pub output_cost_per_token: f64,

    /// The provider. Useful for ironing out minor parameter differences.
    pub litellm_provider: String,

    /// Does this model support system messages? (Not 100% reliable.)
    pub supports_system_messages: Option<bool>,

    /// Does this model support response schema? (Not 100% reliable.)
    pub supports_response_schema: Option<bool>,

    /// Does this model support vision? (Not 100% reliable.)
    pub supports_vision: Option<bool>,

    /// Does this model support function calling? (Not 100% reliable.)
    pub supports_function_calling: Option<bool>,

    /// Does this model support tool choice? (Not 100% reliable.)
    pub supports_tool_choice: Option<bool>,

    /// Does this model support assistant prefill? (Not 100% reliable.)
    pub supports_assistant_prefill: Option<bool>,

    /// Does this model support prompt caching? (Not 100% reliable.)
    pub supports_prompt_caching: Option<bool>,

    /// Does this model support audio input? (Not 100% reliable.)
    pub supports_audio_input: Option<bool>,

    /// Does this model support audio output? (Not 100% reliable.)
    pub supports_audio_output: Option<bool>,

    /// Does this model support PDF input? (Not 100% reliable.)
    pub supports_pdf_input: Option<bool>,

    /// Does this model support embedding image input? (Not 100% reliable.)
    pub supports_embedding_image_input: Option<bool>,

    /// Does this model support native streaming? (Not 100% reliable.)
    pub supports_native_streaming: Option<bool>,

    /// Does this model support web search? (Not 100% reliable.)
    pub supports_web_search: Option<bool>,

    /// Supported API parameters. (Not 100% reliable.)
    #[serde(default)]
    pub supported_openai_params: BTreeSet<String>,

    /// Other parameters.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// LiteLLM error response.
#[derive(Debug, Clone, Deserialize)]
pub struct LiteLlmErrorResponse {
    /// The error detail.
    pub detail: LiteLlmErrorResponseDetail,
}

/// LiteLLM error response detail.
#[derive(Debug, Clone, Deserialize)]
pub struct LiteLlmErrorResponseDetail {
    /// The error code.
    pub error: String,
}

/// LiteLLM success response.
#[derive(Debug, Clone, Deserialize)]
pub struct LiteLlmResponse<T> {
    /// The response data.
    pub data: T,
}

/// Get data for our LiteLLM model cache.
///
/// This will fail for non-LiteLLM endpoints, which is normal. In this case,
/// we fill the cache with an empty map.
#[instrument(level = "debug", skip_all)]
async fn build_model_cache() -> Result<BTreeMap<String, LiteLlmModel>> {
    // TODO: Consider preferring `LITELLM_*` versions of the creds.
    let client_config = get_openai_client_config();
    let client = reqwest::Client::new();

    // Build a URL for the LiteLLM-specific endpoint.
    let mut url = client_config.api_base().to_owned();
    if !url.ends_with('/') {
        url.push('/');
    }
    url.push_str("model/info");

    // Get the model information.
    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to get model information")?;
    let status = response.status();
    if !status.is_success() {
        if let Ok(body) = response.json::<LiteLlmErrorResponse>().await {
            Err(anyhow!(
                "Failed to get model information (status {}): {}",
                status,
                body.detail.error
            ))
        } else {
            Err(anyhow!(
                "Failed to get model information (status {})",
                status
            ))
        }
    } else {
        let response = response
            .json::<LiteLlmResponse<Vec<LiteLlmModel>>>()
            .await
            .context("Failed to parse model information")?;
        let mut model_map = BTreeMap::new();
        for model in response.data {
            model_map.insert(model.model_name.clone(), model);
        }
        Ok(model_map)
    }
}

/// Global model information cache. We fill this once when we first
/// start running.
static LITELLM_MODEL_INFO_CACHE: OnceCell<Option<BTreeMap<String, LiteLlmModel>>> =
    OnceCell::const_new();

/// Get the LiteLLM model information cache.
async fn get_litellm_model_info_cache() -> Option<&'static BTreeMap<String, LiteLlmModel>>
{
    LITELLM_MODEL_INFO_CACHE
        .get_or_init(|| async {
            match build_model_cache().await {
                Ok(model_map) => Some(model_map),
                Err(e) => {
                    debug!("Failed to get model information: {e:?}");
                    None
                }
            }
        })
        .await
        .as_ref()
}

/// Get information about a specific model.
pub async fn litellm_model_info(model_name: &str) -> Option<&'static LiteLlmModel> {
    get_litellm_model_info_cache()
        .await
        .and_then(|cache| cache.get(model_name))
}
