//! Certificate trust for the console: a pinned leaf (self-signed consoles) or webpki roots.
//!
//! Every rejection carries the presented leaf, so callers can tell "not your console" from
//! "untrusted" and `sauron setup` can show the certificate it would pin. A rejected handshake
//! never sends HTTP bytes, so no API key or password reaches the peer.

use std::error::Error as StdError;
use std::fmt;
use std::sync::{Arc, LazyLock};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, OtherError,
    RootCertStore, SignatureScheme,
};

static PROVIDER: LazyLock<Arc<CryptoProvider>> =
    LazyLock::new(|| Arc::new(rustls::crypto::ring::default_provider()));

static WEBPKI: LazyLock<Result<Arc<WebPkiServerVerifier>, String>> = LazyLock::new(|| {
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), PROVIDER.clone())
        .build()
        .map_err(|e| e.to_string())
});

/// SHA-256 of a certificate (DER); shown as uppercase hex separated by colons, as openssl prints it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn of(der: &[u8]) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, der);
        let mut bytes = [0; 32];
        bytes.copy_from_slice(digest.as_ref());
        Self(bytes)
    }

    /// Accepts any case, with or without colons.
    pub fn parse(text: &str) -> Option<Self> {
        let mut bytes = [0; 32];
        let mut digits = text.trim().bytes().filter(|&b| b != b':');
        for byte in &mut bytes {
            let high = (digits.next()? as char).to_digit(16)?;
            let low = (digits.next()? as char).to_digit(16)?;
            *byte = (high << 4 | low) as u8;
        }
        digits.next().is_none().then_some(Self(bytes))
    }

    /// `AB:CD:EF:…:12:34:56:78`: enough to recognise at a glance.
    pub fn short(&self) -> String {
        let hex = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(":")
        };
        format!("{}:…:{}", hex(&self.0[..3]), hex(&self.0[28..]))
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Why the verifier refused the console's certificate.
pub enum Rejection {
    /// A pin is set and this leaf is a different one: another machine.
    Mismatch { leaf: CertificateDer<'static> },
    /// No pin, and webpki doesn't trust the leaf for this name.
    Untrusted {
        leaf: CertificateDer<'static>,
        reason: rustls::Error,
    },
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejection::Mismatch { leaf } => write!(
                f,
                "certificate mismatch (got SHA256 {})",
                Fingerprint::of(leaf)
            ),
            Rejection::Untrusted { reason, .. } => write!(f, "untrusted certificate ({reason})"),
        }
    }
}

// rustls shows `CertificateError::Other` with Debug; keep that readable.
impl fmt::Debug for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl StdError for Rejection {}

/// The rustls error behind `err`. I/O errors are entered through `get_ref`, because their
/// `source()` skips the error they wrap (and they may wrap each other).
pub fn find<'a>(err: &'a (dyn StdError + 'static)) -> Option<&'a rustls::Error> {
    let mut next = Some(err);
    while let Some(err) = next {
        if let Some(tls) = err.downcast_ref::<rustls::Error>() {
            return Some(tls);
        }
        next = match err.downcast_ref::<std::io::Error>() {
            Some(io) => io.get_ref().map(|inner| inner as &(dyn StdError + 'static)),
            None => err.source(),
        };
    }
    None
}

/// The certificate rejection behind `err`, if that is why it failed.
pub fn rejection<'a>(err: &'a (dyn StdError + 'static)) -> Option<&'a Rejection> {
    match find(err)? {
        rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(other))) => {
            other.downcast_ref()
        }
        _ => None,
    }
}

/// rustls client config (ring provider, no ALPN) trusting exactly `pin`, or webpki roots without one.
pub fn client_config(pin: Option<Fingerprint>) -> anyhow::Result<ClientConfig> {
    let trust = match pin {
        Some(pin) => Trust::Pin(pin),
        None => Trust::WebPki(
            WEBPKI
                .as_ref()
                .map_err(|e| anyhow::anyhow!("cannot load root certificates: {e}"))?
                .clone(),
        ),
    };
    let verifier = ConsoleVerifier {
        trust,
        provider: PROVIDER.clone(),
    };
    Ok(ClientConfig::builder_with_provider(PROVIDER.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth())
}

#[derive(Debug)]
enum Trust {
    Pin(Fingerprint),
    WebPki(Arc<WebPkiServerVerifier>),
}

/// Accepts exactly the pinned leaf (ignoring name, chain and dates), else defers to webpki.
/// Handshake signatures are always verified.
#[derive(Debug)]
struct ConsoleVerifier {
    trust: Trust,
    provider: Arc<CryptoProvider>,
}

fn reject(rejection: Rejection) -> rustls::Error {
    rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(rejection))))
}

impl ServerCertVerifier for ConsoleVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match &self.trust {
            Trust::Pin(pin) if Fingerprint::of(end_entity) == *pin => {
                Ok(ServerCertVerified::assertion())
            }
            Trust::Pin(_) => Err(reject(Rejection::Mismatch {
                leaf: end_entity.clone().into_owned(),
            })),
            Trust::WebPki(webpki) => webpki
                .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
                .map_err(|reason| {
                    reject(Rejection::Untrusted {
                        leaf: end_entity.clone().into_owned(),
                        reason,
                    })
                }),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }

    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        match &self.trust {
            Trust::Pin(_) => None,
            Trust::WebPki(webpki) => webpki.root_hint_subjects(),
        }
    }
}

/// Subject CN and whether the certificate is self-issued (issuer == subject), read from its DER.
pub fn describe(cert: &[u8]) -> Option<(Option<String>, bool)> {
    let (_, cert, _) = der(cert)?;
    let (_, tbs, _) = der(cert)?;
    let (tag, _, after_first) = der(tbs)?;
    // `[0] version` is optional; the serial number follows it.
    let after_serial = if tag == 0xa0 {
        der(after_first)?.2
    } else {
        after_first
    };
    let (_, _, rest) = der(after_serial)?; // signature algorithm
    let (_, issuer, rest) = der(rest)?;
    let (_, _, rest) = der(rest)?; // validity
    let (_, subject, _) = der(rest)?;
    Some((common_name(subject), issuer == subject))
}

/// The first commonName (2.5.4.3) in an X.501 Name.
fn common_name(mut name: &[u8]) -> Option<String> {
    while let Some((_, mut set, rest)) = der(name) {
        while let Some((_, attribute, next)) = der(set) {
            let (tag, oid, value) = der(attribute)?;
            if tag == 0x06 && oid == [0x55, 0x04, 0x03] {
                return Some(String::from_utf8_lossy(der(value)?.1).into_owned());
            }
            set = next;
        }
        name = rest;
    }
    None
}

/// One DER element: (tag, contents, remaining input).
fn der(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let (bytes, rest) = rest.split_at(count);
        (
            bytes.iter().fold(0, |len, &b| len << 8 | usize::from(b)),
            rest,
        )
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// SHA-256 of `abc` (FIPS 180-2 test vector).
    const ABC_SHA256: &str = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";

    fn with_colons(hex: &str) -> String {
        hex.as_bytes()
            .chunks(2)
            .map(|pair| std::str::from_utf8(pair).unwrap())
            .collect::<Vec<_>>()
            .join(":")
    }

    #[test]
    fn parse_accepts_any_case_with_or_without_colons() {
        let pin = Fingerprint::parse(ABC_SHA256).unwrap();
        let lower = ABC_SHA256.to_lowercase();
        for text in [
            lower.clone(),
            with_colons(ABC_SHA256),
            with_colons(&lower),
            format!("  {}\n", with_colons(&lower)),
        ] {
            assert_eq!(Fingerprint::parse(&text), Some(pin), "{text:?}");
        }
    }

    #[test]
    fn parse_rejects_wrong_length_and_non_hex() {
        let texts = [
            String::new(),
            ABC_SHA256[..62].to_owned(),
            ABC_SHA256[..63].to_owned(),
            format!("{ABC_SHA256}0"),
            format!("{ABC_SHA256}00"),
            ABC_SHA256.replacen('B', "G", 1),
            ABC_SHA256.replacen("BA", "+A", 1),
            with_colons(ABC_SHA256).replacen(':', " ", 1),
        ];
        for text in texts {
            assert_eq!(Fingerprint::parse(&text), None, "{text:?}");
        }
    }

    #[test]
    fn fingerprint_is_sha256_shown_openssl_style() {
        let pin = Fingerprint::of(b"abc");
        assert_eq!(pin.to_string(), with_colons(ABC_SHA256));
        assert_eq!(Fingerprint::parse(&pin.to_string()), Some(pin));
        assert_eq!(pin.short(), "BA:78:16:…:F2:00:15:AD");
    }

    fn verify_pinned(pin: Fingerprint, leaf: &[u8]) -> Result<ServerCertVerified, rustls::Error> {
        let verifier = ConsoleVerifier {
            trust: Trust::Pin(pin),
            provider: PROVIDER.clone(),
        };
        let name = ServerName::try_from("192.0.2.1").unwrap();
        let now = UnixTime::since_unix_epoch(Duration::from_secs(1_700_000_000));
        verifier.verify_server_cert(&CertificateDer::from(leaf.to_vec()), &[], &name, &[], now)
    }

    #[test]
    fn pin_accepts_exactly_the_pinned_leaf() {
        // Pinning ignores the certificate's contents, so any bytes stand in for a leaf.
        let pin = Fingerprint::of(b"our console");
        assert!(verify_pinned(pin, b"our console").is_ok());

        let err = verify_pinned(pin, b"another machine").unwrap_err();
        match rejection(&err) {
            Some(Rejection::Mismatch { leaf }) => assert_eq!(leaf.as_ref(), b"another machine"),
            other => panic!("expected a mismatch, got {other:?}"),
        }
        // The rejection travels to callers inside (nested) I/O errors, whose `source()` skips it.
        let wrapped = std::io::Error::other(std::io::Error::other(err));
        assert!(matches!(
            rejection(&wrapped),
            Some(Rejection::Mismatch { .. })
        ));
    }

    fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        match content.len() {
            len @ 0..0x80 => out.push(len as u8),
            len @ 0x80..=0xff => out.extend([0x81, len as u8]),
            len => out.extend([0x82, (len >> 8) as u8, len as u8]),
        }
        out.extend_from_slice(content);
        out
    }

    /// An X.501 Name: O=Example, then CN=`cn` if given.
    fn name(cn: Option<&str>) -> Vec<u8> {
        let attribute = |oid: &[u8], value: &str| {
            tlv(
                0x31,
                &tlv(
                    0x30,
                    &[tlv(0x06, oid), tlv(0x0c, value.as_bytes())].concat(),
                ),
            )
        };
        let mut rdns = attribute(&[0x55, 0x04, 0x0a], "Example");
        if let Some(cn) = cn {
            rdns.extend(attribute(&[0x55, 0x04, 0x03], cn));
        }
        tlv(0x30, &rdns)
    }

    fn certificate(v3: bool, issuer: &[u8], subject: &[u8]) -> Vec<u8> {
        let algorithm = tlv(
            0x30,
            &tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02]),
        );
        let mut tbs = Vec::new();
        if v3 {
            tbs.extend(tlv(0xa0, &tlv(0x02, &[2])));
        }
        tbs.extend(tlv(0x02, &[0x01, 0x23]));
        tbs.extend(&algorithm);
        tbs.extend(issuer);
        tbs.extend(tlv(
            0x30,
            &[tlv(0x17, b"250101000000Z"), tlv(0x17, b"350101000000Z")].concat(),
        ));
        tbs.extend(subject);
        tbs.extend(tlv(0x30, &[0; 300])); // key: pushes lengths into the long form
        tlv(
            0x30,
            &[tlv(0x30, &tbs), algorithm, tlv(0x03, &[0; 72])].concat(),
        )
    }

    #[test]
    fn describe_reads_subject_cn_and_self_issued() {
        let console = name(Some("unifi.example"));
        let self_signed = certificate(true, &console, &console);
        assert_eq!(
            describe(&self_signed),
            Some((Some("unifi.example".into()), true))
        );

        let ca_issued_v1 = certificate(false, &name(Some("Example CA")), &console);
        assert_eq!(
            describe(&ca_issued_v1),
            Some((Some("unifi.example".into()), false))
        );

        let no_cn = name(None);
        assert_eq!(
            describe(&certificate(true, &no_cn, &no_cn)),
            Some((None, true))
        );

        assert_eq!(describe(&self_signed[..self_signed.len() / 2]), None);
    }
}
