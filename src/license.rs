use std::collections::HashMap;

use rand::Rng;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

const POLAR_API_BASE: &str = "https://api.polar.sh";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct LicenseCheck {
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    organization_id: Option<String>,
}

pub fn polar_oat() -> String {
    std::env::var("POLAR_API_KEY").expect("missing POLAR_API_KEY")
}

fn is_org_token() -> bool {
    polar_oat().starts_with("polar_oat_")
}

fn org_id_for_request() -> Option<String> {
    if is_org_token() {
        None
    } else {
        std::env::var("POLAR_ORG_ID").ok()
    }
}

pub async fn check_is_pro_user(client: &Client, key: String) -> bool {
    let checker = LicenseCheck {
        key,
        organization_id: org_id_for_request(),
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

pub struct CreatedDiscount {
    pub id: String,
    pub code: String,
}

#[derive(Serialize)]
struct DiscountCreate {
    name: String,
    #[serde(rename = "type")]
    kind: &'static str,
    duration: &'static str,
    basis_points: u32,
    code: String,
    max_redemptions: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    products: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    organization_id: Option<String>,
    metadata: HashMap<&'static str, String>,
}

#[derive(Deserialize)]
struct PolarDiscountResponse {
    id: String,
    code: Option<String>,
}

fn random_code(len: usize) -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
        .collect()
}

fn product_ids() -> Option<Vec<String>> {
    let raw = std::env::var("POLAR_PRODUCT_IDS").ok()?;
    let ids: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    (!ids.is_empty()).then_some(ids)
}

pub async fn create_discount(client: &Client, discord_user_id: i64) -> Result<CreatedDiscount, String> {
    let code = random_code(10);
    let mut metadata = HashMap::new();
    metadata.insert("discord_user_id", discord_user_id.to_string());
    metadata.insert("source", "sxitchbot".to_string());

    let payload = DiscountCreate {
        name: format!("Sxitch Discord 25% Off ({code})"),
        kind: "percentage",
        duration: "once",
        basis_points: 2500,
        code: code.clone(),
        max_redemptions: 1,
        products: product_ids(),
        organization_id: org_id_for_request(),
        metadata,
    };

    let response = client
        .post(format!("{POLAR_API_BASE}/v1/discounts/"))
        .bearer_auth(polar_oat())
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("request to Polar failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let body: String = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect();
        return Err(format!("Polar returned {status}: {body}"));
    }

    let discount: PolarDiscountResponse = response
        .json()
        .await
        .map_err(|e| format!("could not parse Polar response: {e}"))?;

    Ok(CreatedDiscount {
        id: discount.id,
        code: discount.code.unwrap_or(code),
    })
}
