// GeoIP resolution - ip-api.com and MaxMind support
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoRecord {
    pub country: String,
    pub city: Option<String>,
    pub timezone: Option<String>,
    pub asn: Option<String>,
    pub provider: String,
    pub confidence: Option<f32>,
}

#[async_trait]
pub trait GeoProvider: Send + Sync {
    async fn resolve(&self, ip: IpAddr) -> Result<GeoRecord>;
}

pub struct IpApiProvider {
    client: reqwest::Client,
}

impl IpApiProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .proxy(reqwest::Proxy::http("http://127.0.0.1:7890").unwrap())
                .build()
                .unwrap(),
        }
    }
}

#[async_trait]
impl GeoProvider for IpApiProvider {
    async fn resolve(&self, ip: IpAddr) -> Result<GeoRecord> {
        let url = format!(
            "http://ip-api.com/json/{}?fields=status,message,country,city,timezone,as",
            ip
        );

        let resp: IpApiResponse = self.client.get(&url).send().await?.json().await?;

        if resp.status != "success" {
            anyhow::bail!("ip-api error: {}", resp.message.unwrap_or_default());
        }

        Ok(GeoRecord {
            country: resp.country.unwrap_or_default(),
            city: resp.city,
            timezone: resp.timezone,
            asn: resp.as_field,
            provider: "ip-api.com".to_string(),
            confidence: None,
        })
    }
}

#[derive(Debug, Deserialize)]
struct IpApiResponse {
    status: String,
    message: Option<String>,
    country: Option<String>,
    city: Option<String>,
    timezone: Option<String>,
    #[serde(rename = "as")]
    as_field: Option<String>,
}

// TODO: Add MaxMindProvider for local GeoLite2 database
