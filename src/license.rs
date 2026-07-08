use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct LicenseCheck {
    key: String,
    organization_id: String,
}

pub fn org_id() -> String {
    std::env::var("POLAR_ORG_ID").unwrap()
}

pub fn polar_oat() -> String {
    std::env::var("POLAR_API_KEY").unwrap()
}

pub async fn check_is_pro_user(client: &Client, key: String) -> bool {
    let checker = LicenseCheck {
        key,
        organization_id: org_id(),
    };

    let Ok(resp) = client
        .post("https://api.polar.sh/v1/license-keys/validate")
        .json(&checker)
        .bearer_auth(polar_oat())
        .send()
        .await
    else {
        return false;
    };

    resp.status() == StatusCode::OK
}
