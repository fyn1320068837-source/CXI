// GeoIP resolution and the runtime locale profile derived from the exit IP.
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoRecord {
    pub country_code: Option<String>,
    pub country: String,
    pub city: Option<String>,
    pub timezone: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub asn: Option<String>,
    pub provider: String,
    pub confidence: Option<f32>,
}

/// The values that CIM will apply to newly launched instances.
///
/// The profile is deliberately separate from the GeoIP response: country and
/// city are display data, while language/locale are the normalized values
/// passed to child processes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GeoProfile {
    pub source_ip: Option<String>,
    pub country_code: Option<String>,
    pub country: Option<String>,
    pub city: Option<String>,
    pub timezone: Option<String>,
    pub language: Option<String>,
    pub locale: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub asn: Option<String>,
    pub provider: Option<String>,
    pub updated_at: Option<String>,
}

impl GeoProfile {
    pub fn from_record(ip: IpAddr, record: GeoRecord) -> Self {
        let (language, locale) = language_for_country(record.country_code.as_deref());

        Self {
            source_ip: Some(ip.to_string()),
            country_code: record.country_code,
            country: Some(record.country),
            city: record.city,
            timezone: record.timezone,
            language: Some(language.to_string()),
            locale: Some(locale.to_string()),
            latitude: record.latitude,
            longitude: record.longitude,
            asn: record.asn,
            provider: Some(record.provider),
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
        }
    }

    pub fn is_resolved(&self) -> bool {
        self.source_ip.is_some()
    }

    pub fn has_geo(&self) -> bool {
        self.country.is_some() || self.city.is_some() || self.timezone.is_some()
    }
}

pub type SharedGeoProfile = Arc<RwLock<GeoProfile>>;

pub fn shared_profile() -> SharedGeoProfile {
    Arc::new(RwLock::new(GeoProfile::default()))
}

#[async_trait]
pub trait GeoProvider: Send + Sync {
    async fn resolve(&self, ip: IpAddr) -> Result<GeoRecord>;
}

pub struct IpApiProvider {
    client: reqwest::Client,
}

impl IpApiProvider {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            // Resolve the GeoIP host through the same SOCKS5 gateway so the
            // lookup does not reveal the machine's direct network identity.
            .proxy(reqwest::Proxy::all("socks5h://127.0.0.1:7891")?)
            .timeout(std::time::Duration::from_secs(8))
            .build()?;

        Ok(Self { client })
    }
}

#[async_trait]
impl GeoProvider for IpApiProvider {
    async fn resolve(&self, ip: IpAddr) -> Result<GeoRecord> {
        let url = format!(
            "http://ip-api.com/json/{}?fields=status,message,country,countryCode,city,timezone,lat,lon,as",
            ip
        );

        let resp: IpApiResponse = self
            .client
            .get(&url)
            .send()
            .await
            .context("GeoIP request failed")?
            .json()
            .await
            .context("Invalid GeoIP response")?;

        if resp.status != "success" {
            anyhow::bail!("ip-api error: {}", resp.message.unwrap_or_default());
        }

        Ok(GeoRecord {
            country_code: resp.country_code,
            country: resp.country.unwrap_or_default(),
            city: resp.city,
            timezone: resp.timezone,
            latitude: resp.latitude,
            longitude: resp.longitude,
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
    #[serde(rename = "countryCode")]
    country_code: Option<String>,
    city: Option<String>,
    timezone: Option<String>,
    #[serde(rename = "lat")]
    latitude: Option<f64>,
    #[serde(rename = "lon")]
    longitude: Option<f64>,
    #[serde(rename = "as")]
    as_field: Option<String>,
}

/// Map the GeoIP country code to values understood by common macOS/Unix
/// processes and Chromium/VS Code. Unknown countries intentionally fall back
/// to English rather than guessing a locale from the city name.
pub fn language_for_country(country_code: Option<&str>) -> (&'static str, &'static str) {
    let code = country_code.unwrap_or("").to_ascii_uppercase();

    match code.as_str() {
        "CN" => ("zh-CN", "zh_CN.UTF-8"),
        "TW" | "HK" | "MO" => ("zh-TW", "zh_TW.UTF-8"),
        "JP" => ("ja-JP", "ja_JP.UTF-8"),
        "KR" => ("ko-KR", "ko_KR.UTF-8"),
        "DE" | "AT" => ("de-DE", "de_DE.UTF-8"),
        "FR" | "BE" => ("fr-FR", "fr_FR.UTF-8"),
        "ES" | "MX" | "AR" | "CL" | "CO" | "PE" => ("es-ES", "es_ES.UTF-8"),
        "PT" => ("pt-PT", "pt_PT.UTF-8"),
        "BR" => ("pt-BR", "pt_BR.UTF-8"),
        "IT" => ("it-IT", "it_IT.UTF-8"),
        "NL" => ("nl-NL", "nl_NL.UTF-8"),
        "SE" => ("sv-SE", "sv_SE.UTF-8"),
        "NO" => ("nb-NO", "nb_NO.UTF-8"),
        "DK" => ("da-DK", "da_DK.UTF-8"),
        "FI" => ("fi-FI", "fi_FI.UTF-8"),
        "PL" => ("pl-PL", "pl_PL.UTF-8"),
        "TR" => ("tr-TR", "tr_TR.UTF-8"),
        "RU" | "BY" | "KZ" => ("ru-RU", "ru_RU.UTF-8"),
        // These markets commonly use English as the default product locale.
        "US" | "CA" | "GB" | "IE" | "AU" | "NZ" | "SG" | "IN" | "PH" => ("en-US", "en_US.UTF-8"),
        _ => ("en-US", "en_US.UTF-8"),
    }
}

#[cfg(test)]
mod tests {
    use super::{language_for_country, GeoProfile, GeoRecord};
    use std::net::IpAddr;

    #[test]
    fn maps_country_to_locale() {
        assert_eq!(language_for_country(Some("CN")), ("zh-CN", "zh_CN.UTF-8"));
        assert_eq!(language_for_country(Some("JP")), ("ja-JP", "ja_JP.UTF-8"));
        assert_eq!(
            language_for_country(Some("unknown")),
            ("en-US", "en_US.UTF-8")
        );
    }

    #[test]
    fn derives_runtime_profile_from_geo_record() {
        let profile = GeoProfile::from_record(
            "203.0.113.10".parse::<IpAddr>().unwrap(),
            GeoRecord {
                country_code: Some("CN".to_string()),
                country: "China".to_string(),
                city: Some("Shanghai".to_string()),
                timezone: Some("Asia/Shanghai".to_string()),
                latitude: Some(31.23),
                longitude: Some(121.47),
                asn: None,
                provider: "test".to_string(),
                confidence: None,
            },
        );

        assert_eq!(profile.source_ip.as_deref(), Some("203.0.113.10"));
        assert_eq!(profile.language.as_deref(), Some("zh-CN"));
        assert_eq!(profile.locale.as_deref(), Some("zh_CN.UTF-8"));
        assert_eq!(profile.timezone.as_deref(), Some("Asia/Shanghai"));
    }
}
