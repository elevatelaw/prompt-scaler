//! AWS-related code shared by different modules.

use aws_config::BehaviorVersion;
use aws_sdk_textract::config::http::HttpResponse;
use aws_smithy_runtime_api::{
    client::result::SdkError, http::StatusCode as AwsStatusCode,
};
use reqwest::StatusCode;

use crate::{prelude::*, retry::IsKnownTransient};

/// Load the user's AWS configuration using standard conventions.
pub async fn load_aws_config() -> Result<aws_config::SdkConfig> {
    Ok(aws_config::load_defaults(BehaviorVersion::v2025_08_07()).await)
}

impl<ServiceErrorType> IsKnownTransient for SdkError<ServiceErrorType, HttpResponse>
where
    ServiceErrorType: IsKnownTransient,
{
    fn is_known_transient(&self) -> bool {
        match self {
            SdkError::TimeoutError(_) => true,
            SdkError::DispatchFailure(dispatch) => {
                dispatch.is_io() || dispatch.is_timeout()
            }
            SdkError::ResponseError(response) => {
                response.raw().status().is_known_transient()
            }
            SdkError::ServiceError(service_err) => service_err.err().is_known_transient(),
            _ => false,
        }
    }
}

impl IsKnownTransient for AwsStatusCode {
    fn is_known_transient(&self) -> bool {
        // Convert this to a regular `StatusCode`, and use the standard implementation.
        match StatusCode::from_u16(self.as_u16()) {
            Ok(status) => status.is_known_transient(),
            Err(_) => false, // If we can't convert, assume it's not transient.
        }
    }
}
