use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::Value;
use sigstore_verify::bundle::BundleV03;
use sigstore_verify::types::{
    DerCertificate, DsseEnvelope, DsseSignature, KeyId, PayloadBytes, Sha256Hash, SignatureBytes, TransparencyLogEntry,
};
use sigstore_verify::{VerificationPolicy, Verifier};
use x509_cert::Certificate;
use x509_cert::der::Decode as _;
use x509_cert::der::asn1::Utf8StringRef;

const DSSE_PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize)]
pub(super) struct Publisher {
    pub kind: String,
    pub claims: BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct VerificationContext {
    verifier: Arc<Verifier>,
    identity: String,
    issuer: String,
    claims: BTreeMap<String, String>,
}

impl VerificationContext {
    pub(crate) const fn new(
        verifier: Arc<Verifier>,
        identity: String,
        issuer: String,
        claims: BTreeMap<String, String>,
    ) -> Self {
        Self {
            verifier,
            identity,
            issuer,
            claims,
        }
    }

    pub(super) fn verify(&self, attestation: &Value, sha256: &str) -> Result<Publisher, ()> {
        let input: Attestation = serde_json::from_value(attestation.clone()).map_err(|_| ())?;
        let certificate = STANDARD
            .decode(input.verification_material.certificate)
            .map_err(|_| ())?;
        let envelope = DsseEnvelope::new(
            DSSE_PAYLOAD_TYPE.to_owned(),
            PayloadBytes::new(STANDARD.decode(input.envelope.statement).map_err(|_| ())?),
            DsseSignature {
                sig: SignatureBytes::new(STANDARD.decode(input.envelope.signature).map_err(|_| ())?),
                keyid: KeyId::default(),
            },
        );
        let mut bundle = BundleV03::with_certificate_and_dsse(DerCertificate::new(certificate.clone()), envelope);
        for entry in input.verification_material.transparency_entries {
            bundle = bundle.with_tlog_entry(serde_json::from_value::<TransparencyLogEntry>(entry).map_err(|_| ())?);
        }
        let verified = self
            .verifier
            .verify(
                Sha256Hash::from_hex(sha256).map_err(|_| ())?,
                &bundle.into_bundle(),
                &VerificationPolicy::new(&self.identity, &self.issuer),
            )
            .map_err(|_| ())?;
        let mut claims = self.verify_claims(&certificate)?;
        claims.insert("identity".to_owned(), verified.identity().ok_or(())?.to_owned());
        Ok(Publisher {
            kind: verified.issuer().ok_or(())?.to_owned(),
            claims,
        })
    }

    fn verify_claims(&self, certificate: &[u8]) -> Result<BTreeMap<String, String>, ()> {
        let certificate = Certificate::from_der(certificate).map_err(|_| ())?;
        let mut actual = BTreeMap::new();
        for extension in certificate.tbs_certificate.extensions.iter().flatten() {
            let oid = extension.extn_id.to_string();
            if self.claims.contains_key(&oid) {
                let value = Utf8StringRef::from_der(extension.extn_value.as_bytes())
                    .map_err(|_| ())?
                    .to_string();
                actual.insert(oid, value).is_none().then_some(()).ok_or(())?;
            }
        }
        (actual == self.claims).then_some(actual).ok_or(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attestation {
    #[serde(rename = "version")]
    _version: u64,
    verification_material: VerificationMaterial,
    envelope: Envelope,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerificationMaterial {
    certificate: String,
    transparency_entries: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    statement: String,
    signature: String,
}
