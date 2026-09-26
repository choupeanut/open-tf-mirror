use anyhow::{Result, bail};
use semver::Version;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveName {
    pub provider_type: String,
    pub version: String,
    pub os: String,
    pub arch: String,
}

impl ArchiveName {
    pub fn parse(route_type: &str, archive: &str) -> Result<Self> {
        let prefix = format!("terraform-provider-{route_type}");
        let remainder = archive
            .strip_prefix(&prefix)
            .ok_or_else(|| anyhow::anyhow!("invalid type"))?;
        let remainder = remainder
            .strip_suffix(".zip")
            .ok_or_else(|| anyhow::anyhow!("invalid archive"))?;

        if let Some(remainder) = remainder.strip_prefix('_') {
            let mut fields = remainder.rsplitn(3, '_');
            let arch = fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("invalid archive"))?;
            let os = fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("invalid archive"))?;
            let version = fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("invalid archive"))?;
            return Self::from_parts(route_type, version, os, arch);
        }

        let remainder = remainder
            .strip_prefix('-')
            .ok_or_else(|| anyhow::anyhow!("invalid archive"))?;
        let remainder = remainder
            .strip_suffix("-bin")
            .ok_or_else(|| anyhow::anyhow!("invalid archive suffix"))?;
        let mut fields = remainder.rsplitn(3, '-');
        let arch = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid archive"))?;
        let os = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid archive"))?;
        let version = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid archive"))?;
        Self::from_parts(route_type, version, os, arch)
    }

    fn from_parts(route_type: &str, version: &str, os: &str, arch: &str) -> Result<Self> {
        if !os.bytes().all(|byte| byte.is_ascii_lowercase())
            || !arch
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            bail!("invalid platform");
        }
        let version = version.strip_prefix('v').unwrap_or(version);
        Version::parse(version).map_err(|_| anyhow::anyhow!("invalid semantic version"))?;
        Ok(Self {
            provider_type: route_type.to_string(),
            version: version.to_string(),
            os: os.to_string(),
            arch: arch.to_string(),
        })
    }
}
