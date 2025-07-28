//! AWS-related code shared by different modules.

use aws_config::BehaviorVersion;

use crate::prelude::*;

/// Load the user's AWS configuration using standard conventions.
pub async fn load_aws_config() -> Result<aws_config::SdkConfig> {
    Ok(aws_config::load_defaults(BehaviorVersion::v2025_01_17()).await)
}
