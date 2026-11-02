//! The Agent's auth: the API key goes to BoxLite as a secret scoped to the base URL's host, so
//! the guest only ever holds a placeholder (jonaswre-labs/boxlite-quick#14).

use crate::config::{BaseUrl, Config};
use crate::digest::sha256_hex;
use anyhow::Context;
use boxlite::runtime::options::{BoxOptions, Secret};

/// What the guest holds in place of the API key. BoxLite swaps it for the key in HTTPS
/// requests to the base URL's host.
const PLACEHOLDER: &str = "<BOXLITE_SECRET:anthropic>";

/// The base URL and API key a Sandbox authenticates with. Its `Debug` output redacts the key.
#[derive(Debug)]
pub struct Auth {
    base_url: BaseUrl,
    secret: Secret,
}

impl Auth {
    /// The merged config's base URL and API key.
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        let base_url = config
            .base_url
            .as_ref()
            .context("no base URL is set: set `base_url` in the Global Config, or BOXQ_BASE_URL")?;
        let api_key = config.api_key.as_ref().context(
            "no API key is set: set `api_key` in boxq's credentials file, or BOXQ_API_KEY",
        )?;
        Ok(Self::new(base_url, api_key.expose()))
    }

    pub fn new(base_url: &BaseUrl, api_key: &str) -> Self {
        Self {
            base_url: base_url.clone(),
            secret: Secret {
                name: "anthropic".into(),
                hosts: vec![base_url.host().into()],
                placeholder: PLACEHOLDER.into(),
                value: api_key.into(),
            },
        }
    }

    /// Gives a Box created with `options` the key as a secret, and the guest the env that
    /// points Claude Code at the base URL with the placeholder.
    pub fn apply(&self, options: &mut BoxOptions) {
        options.secrets.push(self.secret.clone());
        options.env.extend([
            ("ANTHROPIC_BASE_URL".into(), self.base_url.as_str().into()),
            ("ANTHROPIC_AUTH_TOKEN".into(), PLACEHOLDER.into()),
            // Updates, telemetry, error reports and the plugin marketplace would each need
            // their own egress, and the Agent works without them (jonaswre-labs/boxlite-quick#3).
            (
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
                "1".into(),
            ),
            (
                "CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL".into(),
                "1".into(),
            ),
            ("DISABLE_UPDATES".into(), "1".into()),
        ]);
    }

    /// A digest that tells two Auths apart without holding the key. BoxLite's own database,
    /// in the same home, already holds the key itself, so this exposes nothing new.
    pub(crate) fn fingerprint(&self) -> String {
        sha256_hex(&[
            self.base_url.as_str().as_bytes(),
            &[0],
            self.secret.value.as_bytes(),
        ])
    }
}
