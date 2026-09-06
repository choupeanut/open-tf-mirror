use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use parking_lot::RwLock;
use rustls::{
    crypto::aws_lc_rs::sign::any_supported_type,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};

#[derive(Debug)]
pub struct ReloadingCertResolver {
    cert_path: PathBuf,
    key_path: PathBuf,
    cached_key: RwLock<(CachedCertifiedKey, Instant)>,
    reload_interval: Duration,
}

#[derive(Debug)]
struct CachedCertifiedKey {
    key: Arc<CertifiedKey>,
    not_after: i64,
}

impl ReloadingCertResolver {
    pub fn new(cert_path: impl AsRef<Path>, key_path: impl AsRef<Path>) -> Result<Self> {
        Self::new_with_reload_interval(cert_path, key_path, Duration::from_secs(5))
    }

    pub fn new_with_reload_interval(
        cert_path: impl AsRef<Path>,
        key_path: impl AsRef<Path>,
        reload_interval: Duration,
    ) -> Result<Self> {
        let cert_path = cert_path.as_ref().to_path_buf();
        let key_path = key_path.as_ref().to_path_buf();
        let loaded = load_certified_key(&cert_path, &key_path)?;
        Ok(Self {
            cert_path,
            key_path,
            cached_key: RwLock::new((
                CachedCertifiedKey {
                    key: Arc::new(loaded.key),
                    not_after: loaded.not_after,
                },
                Instant::now(),
            )),
            reload_interval,
        })
    }

    pub fn resolve_current_cert(&self) -> Arc<CertifiedKey> {
        self.resolve_current_cert_option()
            .expect("TLS certificate expired and no valid replacement is available")
    }

    fn resolve_current_cert_option(&self) -> Option<Arc<CertifiedKey>> {
        let now = Instant::now();
        let now_epoch = unix_timestamp(SystemTime::now());
        {
            let cached = self.cached_key.read();
            if now_epoch < cached.0.not_after && now.duration_since(cached.1) < self.reload_interval
            {
                return Some(Arc::clone(&cached.0.key));
            }
        }

        match self.load_certified_key() {
            Ok(loaded) => {
                let key = Arc::new(loaded.key);
                *self.cached_key.write() = (
                    CachedCertifiedKey {
                        key: Arc::clone(&key),
                        not_after: loaded.not_after,
                    },
                    now,
                );
                Some(key)
            }
            Err(err) => {
                tracing::warn!(
                    cert_path = %self.cert_path.display(),
                    key_path = %self.key_path.display(),
                    error = %err,
                    "failed to reload TLS certificate, falling back to cached certificate"
                );
                let mut cached = self.cached_key.write();
                cached.1 = now;
                if now_epoch < cached.0.not_after {
                    Some(Arc::clone(&cached.0.key))
                } else {
                    tracing::error!(
                        cert_path = %self.cert_path.display(),
                        error = %err,
                        "cached TLS certificate has expired and cannot be used"
                    );
                    None
                }
            }
        }
    }

    fn load_certified_key(&self) -> Result<LoadedCertifiedKey> {
        load_certified_key(&self.cert_path, &self.key_path)
    }
}

impl ResolvesServerCert for ReloadingCertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.resolve_current_cert_option()
    }
}

#[derive(Debug)]
struct LoadedCertifiedKey {
    key: CertifiedKey,
    not_after: i64,
}

fn load_certified_key(cert_path: &Path, key_path: &Path) -> Result<LoadedCertifiedKey> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .with_context(|| format!("open TLS certificate {}", cert_path.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parse TLS certificate {}", cert_path.display()))?;
    if certs.is_empty() {
        bail!("TLS certificate file did not contain a certificate chain");
    }

    let not_after = validate_certificate_validity(&certs)?;

    let key = PrivateKeyDer::from_pem_file(key_path)
        .with_context(|| format!("parse TLS private key {}", key_path.display()))?;

    Ok(LoadedCertifiedKey {
        key: build_certified_key(certs, key)?,
        not_after,
    })
}

fn build_certified_key(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<CertifiedKey> {
    let signing_key = any_supported_type(&key).context("unsupported TLS private key type")?;
    let certified_key = CertifiedKey::new(certs, signing_key);
    certified_key
        .keys_match()
        .context("TLS certificate and private key do not match")?;
    Ok(certified_key)
}

fn validate_certificate_validity(certs: &[CertificateDer<'_>]) -> Result<i64> {
    let now = unix_timestamp(SystemTime::now());
    let mut earliest_not_after = i64::MAX;
    for (index, cert) in certs.iter().enumerate() {
        let (not_before, not_after) = parse_certificate_validity(cert.as_ref())
            .with_context(|| format!("parse validity of TLS certificate {index}"))?;

        if now < not_before {
            bail!("TLS certificate {index} is not valid yet (not_before={not_before}, now={now})");
        }
        // X.509 `notAfter` is an exclusive upper bound.
        if now >= not_after {
            bail!("TLS certificate {index} has expired (not_after={not_after}, now={now})");
        }
        earliest_not_after = earliest_not_after.min(not_after);
    }
    Ok(earliest_not_after)
}

fn parse_certificate_validity(der: &[u8]) -> Result<(i64, i64)> {
    let mut input = der;
    let certificate = read_der_value(&mut input, 0x30, "certificate")?;
    if !input.is_empty() {
        bail!("TLS certificate contains trailing data");
    }

    let mut certificate_fields = certificate;
    let tbs = read_der_value(&mut certificate_fields, 0x30, "TBSCertificate")?;
    let _ = read_der_value(
        &mut certificate_fields,
        0x30,
        "certificate signature algorithm",
    )?;
    let _ = read_der_value(&mut certificate_fields, 0x03, "certificate signature")?;
    if !certificate_fields.is_empty() {
        bail!("TLS certificate contains trailing fields");
    }

    let mut tbs_fields = tbs;
    if tbs_fields.first().copied() == Some(0xa0) {
        let _ = read_der_value(&mut tbs_fields, 0xa0, "TBSCertificate version")?;
    }
    let _ = read_der_value(&mut tbs_fields, 0x02, "certificate serial number")?;
    let _ = read_der_value(&mut tbs_fields, 0x30, "certificate signature algorithm")?;
    let _ = read_der_value(&mut tbs_fields, 0x30, "certificate issuer")?;
    let mut validity = read_der_value(&mut tbs_fields, 0x30, "certificate validity")?;
    let (not_before_tag, not_before_der) = read_der_tlv(&mut validity, "certificate not_before")?;
    let (not_after_tag, not_after_der) = read_der_tlv(&mut validity, "certificate not_after")?;
    if !validity.is_empty() {
        bail!("certificate validity contains trailing data");
    }

    let not_before = parse_certificate_time(not_before_tag, not_before_der, "not_before")?;
    let not_after = parse_certificate_time(not_after_tag, not_after_der, "not_after")?;
    if not_before > not_after {
        bail!("certificate not_before is later than not_after");
    }
    Ok((not_before, not_after))
}

fn read_der_value<'a>(input: &mut &'a [u8], expected_tag: u8, label: &str) -> Result<&'a [u8]> {
    let (tag, value) = read_der_tlv(input, label)?;
    if tag != expected_tag {
        bail!("{label} has unexpected DER tag 0x{tag:02x}, expected 0x{expected_tag:02x}");
    }
    Ok(value)
}

fn read_der_tlv<'a>(input: &mut &'a [u8], label: &str) -> Result<(u8, &'a [u8])> {
    let tag = *input
        .first()
        .with_context(|| format!("{label} is missing a DER tag"))?;
    *input = &input[1..];
    let first_length = *input
        .first()
        .with_context(|| format!("{label} is missing a DER length"))?;
    *input = &input[1..];

    let length = if first_length & 0x80 == 0 {
        usize::from(first_length)
    } else {
        let length_bytes = usize::from(first_length & 0x7f);
        if length_bytes == 0 {
            bail!("{label} uses an indefinite DER length");
        }
        if length_bytes > std::mem::size_of::<usize>() || length_bytes > input.len() {
            bail!("{label} has an invalid DER length");
        }
        let mut length = 0usize;
        for byte in &input[..length_bytes] {
            length = length
                .checked_shl(8)
                .and_then(|value| value.checked_add(usize::from(*byte)))
                .with_context(|| format!("{label} DER length overflows usize"))?;
        }
        *input = &input[length_bytes..];
        length
    };

    if length > input.len() {
        bail!("{label} is truncated");
    }
    let (value, remainder) = input.split_at(length);
    *input = remainder;
    Ok((tag, value))
}

fn parse_certificate_time(tag: u8, value: &[u8], label: &str) -> Result<i64> {
    if value.last().copied() != Some(b'Z') {
        bail!("certificate {label} must use UTC (Z) time");
    }
    let value = &value[..value.len() - 1];
    let (year, fields) = match tag {
        0x17 => {
            if value.len() != 12 {
                bail!("certificate {label} has an invalid UTCTime");
            }
            let year = parse_digits(&value[..2], label)?;
            (
                if year >= 50 { 1900 + year } else { 2000 + year },
                &value[2..],
            )
        }
        0x18 => {
            if value.len() < 14 {
                bail!("certificate {label} has an invalid GeneralizedTime");
            }
            (parse_digits(&value[..4], label)?, &value[4..])
        }
        _ => bail!("certificate {label} has unsupported DER time tag 0x{tag:02x}"),
    };

    if fields.len() < 10 {
        bail!("certificate {label} is missing time fields");
    }
    let month = parse_digits(&fields[..2], label)?;
    let day = parse_digits(&fields[2..4], label)?;
    let hour = parse_digits(&fields[4..6], label)?;
    let minute = parse_digits(&fields[6..8], label)?;
    let second = parse_digits(&fields[8..10], label)?;
    if fields.len() > 10
        && (fields[10] != b'.'
            || fields.len() == 11
            || fields[11..].iter().any(|byte| !byte.is_ascii_digit()))
    {
        bail!("certificate {label} has invalid fractional seconds");
    }
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        bail!("certificate {label} contains an invalid calendar value");
    }

    let days = days_from_civil(year, month, day)?;
    days.checked_mul(86_400)
        .and_then(|value| value.checked_add(hour * 3_600))
        .and_then(|value| value.checked_add(minute * 60))
        .and_then(|value| value.checked_add(second))
        .with_context(|| format!("certificate {label} timestamp overflows i64"))
}

fn parse_digits(value: &[u8], label: &str) -> Result<i64> {
    if value.is_empty() || value.iter().any(|byte| !byte.is_ascii_digit()) {
        bail!("certificate {label} contains non-digit time fields");
    }
    value.iter().try_fold(0i64, |value, byte| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(i64::from(byte - b'0')))
            .with_context(|| format!("certificate {label} time field overflows i64"))
    })
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

// Howard Hinnant's proleptic Gregorian calendar conversion, with the Unix
// epoch (1970-01-01) as day zero.
fn days_from_civil(year: i64, month: i64, day: i64) -> Result<i64> {
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year / 400
    } else {
        (adjusted_year - 399) / 400
    };
    let year_of_era = adjusted_year - era * 400;
    let month_of_year = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_of_year + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.checked_mul(146_097)
        .and_then(|value| value.checked_add(day_of_era))
        .and_then(|value| value.checked_sub(719_468))
        .with_context(|| "certificate date overflows i64")
}

fn unix_timestamp(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs().min(i64::MAX as u64) as i64,
        Err(error) => -(error.duration().as_secs().min(i64::MAX as u64) as i64),
    }
}
