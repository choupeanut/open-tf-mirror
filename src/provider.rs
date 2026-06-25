use anyhow::{Result, bail};
use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveName {
    pub provider_type: String,
    pub version: String,
    pub os: String,
    pub arch: String,
}

impl ArchiveName {
    pub fn parse(route_type: &str, archive: &str) -> Result<Self> {
        let re = Regex::new(
            r"^terraform-provider-(?P<type>[\w-]+)[_-](?P<version>[\w|\.]+)[_-](?P<os>[a-z]+)[_-](?P<arch>[a-z0-9]+)([_-].*)?\.zip$",
        )
        .expect("provider archive regex must compile");

        let captures = re
            .captures(archive)
            .ok_or_else(|| anyhow::anyhow!("invalid archive"))?;

        let provider_type = captures["type"].to_string();
        if provider_type != route_type {
            bail!("invalid type");
        }

        Ok(Self {
            provider_type,
            version: captures["version"].trim_start_matches('v').to_string(),
            os: captures["os"].to_string(),
            arch: captures["arch"].to_string(),
        })
    }
}
