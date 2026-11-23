//! APK Signature Scheme v2 verification (design doc §03 step 3).
//!
//! Layout: `[entries][APK Signing Block][central directory][EOCD]`.
//! The signing block: `u64 size | pairs | u64 size | "APK Sig Block 42"`.
//! v2 pair id = 0x7109871a, v3 = 0xf05368c0.

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};
use sha2::{Digest, Sha256, Sha512};
use x509_parser::prelude::FromDer;

use crate::zip::{find_central_directory, CentralDirectory};

const SIG_BLOCK_MAGIC: &[u8; 16] = b"APK Sig Block 42";
const ID_V2: u32 = 0x7109_871a;
const ID_V3: u32 = 0xf053_68c0;
const ID_V31: u32 = 0x1b93_ad61;
const ID_STAMP: u32 = 0x6dff_800d;

fn sig_err(cause: impl Into<String>) -> OveloError {
    OveloError::new(ErrorCategory::PkgSignature, Phase::Install, cause)
}

/// Signature algorithm IDs from the v2 scheme spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigAlg {
    /// RSA-PSS with SHA-256.
    RsaPssSha256,
    /// RSA-PSS with SHA-512.
    RsaPssSha512,
    /// RSA PKCS#1 v1.5 with SHA-256.
    RsaPkcs1Sha256,
    /// RSA PKCS#1 v1.5 with SHA-512.
    RsaPkcs1Sha512,
    /// ECDSA P-256 with SHA-256.
    EcdsaSha256,
    /// ECDSA P-384 with SHA-512.
    EcdsaSha512,
    /// DSA with SHA-256.
    DsaSha256,
}

impl SigAlg {
    fn from_id(id: u32) -> Option<Self> {
        Some(match id {
            0x0101 => Self::RsaPssSha256,
            0x0102 => Self::RsaPssSha512,
            0x0103 => Self::RsaPkcs1Sha256,
            0x0104 => Self::RsaPkcs1Sha512,
            0x0201 => Self::EcdsaSha256,
            0x0202 => Self::EcdsaSha512,
            0x0301 => Self::DsaSha256,
            _ => return None,
        })
    }

    /// Content digest algorithm for this signature algorithm.
    fn digest_alg(self) -> u32 {
        match self {
            Self::RsaPssSha256 | Self::RsaPkcs1Sha256 | Self::EcdsaSha256 | Self::DsaSha256 => 1,
            Self::RsaPssSha512 | Self::RsaPkcs1Sha512 | Self::EcdsaSha512 => 2,
        }
    }
}

/// A verified APK signer.
#[derive(Debug, Clone)]
pub struct SignerInfo {
    /// SHA-256 fingerprint (hex) of the signer's certificate DER — the
    /// identity used for same-signer update checks.
    pub cert_sha256: String,
    /// Signature algorithm id used.
    pub sig_alg: u32,
    /// Certificate subject (display only).
    pub subject: String,
}

struct Lp<'a> {
    b: &'a [u8],
    pos: usize,
}
impl<'a> Lp<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }
    /// Read one u32-length-prefixed item.
    fn item(&mut self) -> Result<&'a [u8]> {
        if self.pos + 4 > self.b.len() {
            return Err(sig_err("sig.lp.truncated"));
        }
        let len = u32::from_le_bytes(self.b[self.pos..self.pos + 4].try_into().unwrap()) as usize;
        let start = self.pos + 4;
        let end = start
            .checked_add(len)
            .filter(|e| *e <= self.b.len())
            .ok_or_else(|| sig_err("sig.lp.truncated"))?;
        self.pos = end;
        Ok(&self.b[start..end])
    }
    fn done(&self) -> bool {
        self.pos >= self.b.len()
    }
}

/// Locate the APK Signing Block between ZIP content and the central directory.
pub fn find_signing_block(bytes: &[u8]) -> Result<(usize, usize)> {
    let cd = find_central_directory(bytes)?;
    let cd_off = cd.offset as usize;
    if cd_off < 24 {
        return Err(sig_err("sig.block.missing"));
    }
    let magic_at = cd_off - 16;
    if &bytes[magic_at..cd_off] != SIG_BLOCK_MAGIC {
        return Err(sig_err("sig.block.missing"));
    }
    let size2 = u64::from_le_bytes(bytes[cd_off - 24..cd_off - 16].try_into().unwrap()) as usize;
    // size counts everything after the first 8-byte size field:
    // pairs + second size + magic → block starts size2 bytes before cd_off - 8.
    let start = cd_off
        .checked_sub(size2 + 8)
        .ok_or_else(|| sig_err("sig.block.bounds"))?;
    let size1 = u64::from_le_bytes(bytes[start..start + 8].try_into().unwrap()) as usize;
    if size1 != size2 {
        return Err(sig_err("sig.block.size_mismatch"));
    }
    Ok((start, cd_off - 24))
}

/// Iterate (id, value) pairs of the signing block.
pub fn signing_block_pairs(bytes: &[u8]) -> Result<Vec<(u32, &[u8])>> {
    let (start, end) = find_signing_block(bytes)?;
    let mut out = Vec::new();
    let mut pos = start + 8; // skip the first size field
    while pos + 8 <= end {
        let len = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap()) as usize;
        // Pair end must be checked with overflow-safe arithmetic.
        let pair_end = pos
            .checked_add(8)
            .and_then(|p| p.checked_add(len))
            .ok_or_else(|| sig_err("sig.pair.bounds"))?;
        if len < 4 || pair_end > end {
            return Err(sig_err("sig.pair.bounds"));
        }
        let id = u32::from_le_bytes(bytes[pos + 8..pos + 12].try_into().unwrap());
        out.push((id, &bytes[pos + 12..pair_end]));
        pos = pair_end;
    }
    if pos != end {
        return Err(sig_err("sig.pair.bounds"));
    }
    Ok(out)
}

/// Whether a v2 signing block is present.
pub fn has_v2_signature(bytes: &[u8]) -> bool {
    signing_block_pairs(bytes)
        .map(|pairs| pairs.iter().any(|(id, _)| *id == ID_V2))
        .unwrap_or(false)
}

/// Whether a v3/v3.1 block is present (parsed but not yet verified).
pub fn has_v3_signature(bytes: &[u8]) -> bool {
    signing_block_pairs(bytes)
        .map(|pairs| pairs.iter().any(|(id, _)| *id == ID_V3 || *id == ID_V31))
        .unwrap_or(false)
}

/// Source Stamp presence (informational).
pub fn has_stamp(bytes: &[u8]) -> bool {
    signing_block_pairs(bytes)
        .map(|pairs| pairs.iter().any(|(id, _)| *id == ID_STAMP))
        .unwrap_or(false)
}

const CHUNK: usize = 1024 * 1024;

fn chunked_digest<F, H>(sections: &[&[u8]], mut mk: F) -> Vec<u8>
where
    F: FnMut() -> H,
    H: Digest,
{
    let mut parts: Vec<Vec<u8>> = Vec::new();
    for section in sections {
        for chunk in section.chunks(CHUNK) {
            let mut h = mk();
            h.update([0xa5]);
            h.update((chunk.len() as u32).to_le_bytes());
            h.update(chunk);
            parts.push(h.finalize().to_vec());
        }
    }
    let mut h = mk();
    h.update([0x5a]);
    h.update((parts.len() as u32).to_le_bytes());
    for p in &parts {
        h.update(p);
    }
    h.finalize().to_vec()
}

/// Compute the v2 content digest for the given digest algorithm (1=SHA-256,
/// 2=SHA-512). `sig_block_start` replaces the CD-offset field inside the
/// EOCD per the spec.
pub fn content_digest(
    bytes: &[u8],
    cd: &CentralDirectory,
    sig_block_start: usize,
    digest_alg: u32,
) -> Result<Vec<u8>> {
    let (eocd_pos, _) = {
        // Re-locate EOCD to rebuild the adjusted copy.
        let mut found = None;
        let start = bytes.len().saturating_sub(64 * 1024 + 22);
        for i in (start..=bytes.len().saturating_sub(22)).rev() {
            if bytes.len() >= 22
                && u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) == 0x0605_4b50
                && i + 22 + u16::from_le_bytes(bytes[i + 20..i + 22].try_into().unwrap()) as usize
                    == bytes.len()
            {
                found = Some(i);
                break;
            }
        }
        (found.ok_or_else(|| sig_err("sig.eocd.missing"))?, ())
    };
    let mut eocd = bytes[eocd_pos..].to_vec();
    eocd[16..20].copy_from_slice(&(sig_block_start as u32).to_le_bytes());

    // Section 1 ends at the signing block start: the CD physically follows
    // the block, so bytes between them are not part of the digest.
    let before_cd = &bytes[..sig_block_start];
    let cd_bytes = &bytes[cd.offset as usize..(cd.offset + cd.size) as usize];
    let sections: [&[u8]; 3] = [before_cd, cd_bytes, &eocd];
    Ok(match digest_alg {
        1 => chunked_digest(&sections, Sha256::new),
        2 => chunked_digest(&sections, Sha512::new),
        _ => return Err(sig_err("sig.digest_alg.unsupported")),
    })
}

fn verify_signature_bytes(
    alg: SigAlg,
    public_key_der: &[u8],
    signed_data: &[u8],
    signature: &[u8],
) -> Result<()> {
    use rsa::pkcs8::DecodePublicKey;
    match alg {
        SigAlg::RsaPssSha256 | SigAlg::RsaPssSha512 => {
            let key = rsa::RsaPublicKey::from_public_key_der(public_key_der)
                .map_err(|_| sig_err("sig.key.rsa"))?;
            let (pss, hashed) = if alg == SigAlg::RsaPssSha256 {
                (rsa::pss::Pss::new::<Sha256>(), sha256(signed_data))
            } else {
                (rsa::pss::Pss::new::<Sha512>(), sha512(signed_data))
            };
            key.verify(pss, &hashed, signature)
                .map_err(|_| sig_err("sig.verify.failed"))
        }
        SigAlg::RsaPkcs1Sha256 | SigAlg::RsaPkcs1Sha512 => {
            let key = rsa::RsaPublicKey::from_public_key_der(public_key_der)
                .map_err(|_| sig_err("sig.key.rsa"))?;
            let res = if alg == SigAlg::RsaPkcs1Sha256 {
                key.verify(
                    rsa::Pkcs1v15Sign::new::<Sha256>(),
                    &sha256(signed_data),
                    signature,
                )
            } else {
                key.verify(
                    rsa::Pkcs1v15Sign::new::<Sha512>(),
                    &sha512(signed_data),
                    signature,
                )
            };
            res.map_err(|_| sig_err("sig.verify.failed"))
        }
        SigAlg::EcdsaSha256 => {
            use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
            let key = VerifyingKey::from_public_key_der(public_key_der)
                .map_err(|_| sig_err("sig.key.ec"))?;
            let sig =
                Signature::from_der(signature).map_err(|_| sig_err("sig.signature.malformed"))?;
            key.verify(signed_data, &sig)
                .map_err(|_| sig_err("sig.verify.failed"))
        }
        SigAlg::EcdsaSha512 => {
            use p384::ecdsa::{Signature, VerifyingKey};
            let key = VerifyingKey::from_public_key_der(public_key_der)
                .map_err(|_| sig_err("sig.key.ec"))?;
            let sig =
                Signature::from_der(signature).map_err(|_| sig_err("sig.signature.malformed"))?;
            // ECDSA-with-SHA-512: the verifier hashes with the key's
            // paired digest (sha384 for p384), but the APK signature
            // scheme signs a SHA-512 digest — verify the pre-hashed
            // message via the PrehashVerifier.
            use p384::ecdsa::signature::hazmat::PrehashVerifier;
            key.verify_prehash(&sha512(signed_data), &sig)
                .map_err(|_| sig_err("sig.verify.failed"))
        }
        _ => Err(sig_err("sig.alg.unsupported")),
    }
}

fn sha256(b: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    Sha256::digest(b).to_vec()
}
fn sha512(b: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    Sha512::digest(b).to_vec()
}

struct Signer {
    signed_data: Vec<u8>,
    signatures: Vec<(u32, Vec<u8>)>,
    public_key: Vec<u8>,
    digests: Vec<(u32, Vec<u8>)>,
    certs: Vec<Vec<u8>>,
}

fn parse_signers(v2_value: &[u8]) -> Result<Vec<Signer>> {
    let mut seq = Lp::new(v2_value);
    let signers_body = seq.item()?;
    if !seq.done() {
        return Err(sig_err("sig.trailing"));
    }
    let mut signer_seq = Lp::new(signers_body);
    let mut signers = Vec::new();
    while !signer_seq.done() {
        let signer = signer_seq.item()?;
        let mut r = Lp::new(signer);
        let signed_data = r.item()?.to_vec();
        let signatures = r.item()?;
        let public_key = r.item()?.to_vec();

        // signatures: sequence of length-prefixed records, each
        // `u32 recLen || u32 algId || u32 sigLen || sig` — the record
        // wraps `algId || lp(sig)`, so recLen = sigLen + 8.
        let mut sig_list = Vec::new();
        let mut sp = 0usize;
        while sp + 12 <= signatures.len() {
            let rlen = u32::from_le_bytes(signatures[sp..sp + 4].try_into().unwrap()) as usize;
            if rlen < 8 || sp + 4 + rlen > signatures.len() {
                return Err(sig_err("sig.record.truncated"));
            }
            let alg = u32::from_le_bytes(signatures[sp + 4..sp + 8].try_into().unwrap());
            let slen = u32::from_le_bytes(signatures[sp + 8..sp + 12].try_into().unwrap()) as usize;
            if slen + 8 != rlen {
                return Err(sig_err("sig.record.len"));
            }
            sig_list.push((alg, signatures[sp + 12..sp + 4 + rlen].to_vec()));
            sp += 4 + rlen;
        }
        if sp != signatures.len() {
            return Err(sig_err("sig.trailing"));
        }

        // signedData: lp(digests) || lp(certs) || lp(attrs)
        let mut sd = Lp::new(&signed_data);
        let digests_seq = sd.item()?;
        let certs_seq = sd.item()?;
        let _attrs = sd.item().unwrap_or(&[]);
        // digests: sequence of length-prefixed records, each
        // `u32 recLen || u32 sigAlgId || u32 digestLen || digest`
        // (recLen = digestLen + 8).
        let mut digests = Vec::new();
        let mut dp = 0usize;
        while dp + 12 <= digests_seq.len() {
            let rlen = u32::from_le_bytes(digests_seq[dp..dp + 4].try_into().unwrap()) as usize;
            if rlen < 8 || dp + 4 + rlen > digests_seq.len() {
                return Err(sig_err("sig.record.truncated"));
            }
            let alg = u32::from_le_bytes(digests_seq[dp + 4..dp + 8].try_into().unwrap());
            let dlen =
                u32::from_le_bytes(digests_seq[dp + 8..dp + 12].try_into().unwrap()) as usize;
            if dlen + 8 != rlen {
                return Err(sig_err("sig.record.len"));
            }
            // Digest records exist for every alg the signer used — AOSP
            // ignores algs it doesn't support as long as the alg chosen
            // for verification has one, so skip unknown ids.
            let Some(a) = SigAlg::from_id(alg) else {
                dp += 4 + rlen;
                continue;
            };
            let expected = match a.digest_alg() {
                1 => 32,
                2 => 64,
                _ => return Err(sig_err("sig.digest_alg.unsupported")),
            };
            if dlen != expected {
                return Err(sig_err("sig.digest.len"));
            }
            digests.push((alg, digests_seq[dp + 12..dp + 4 + rlen].to_vec()));
            dp += 4 + rlen;
        }
        if dp != digests_seq.len() {
            return Err(sig_err("sig.trailing"));
        }
        let mut certs = Vec::new();
        let mut cseq = Lp::new(certs_seq);
        while !cseq.done() {
            certs.push(cseq.item()?.to_vec());
        }
        if certs.is_empty() {
            return Err(sig_err("sig.no_certificates"));
        }
        signers.push(Signer {
            signed_data,
            signatures: sig_list,
            public_key,
            digests,
            certs,
        });
    }
    Ok(signers)
}

/// Verify all v2 signers. Returns signer info for each.
pub fn verify_v2(bytes: &[u8]) -> Result<Vec<SignerInfo>> {
    let cd = find_central_directory(bytes)?;
    let (block_start, _) = find_signing_block(bytes)?;
    let pairs = signing_block_pairs(bytes)?;
    let v2 = pairs
        .iter()
        .find(|(id, _)| *id == ID_V2)
        .map(|(_, v)| *v)
        .ok_or_else(|| sig_err("sig.v2.absent"))?;
    let signers = parse_signers(v2)?;
    if signers.is_empty() {
        return Err(sig_err("sig.no_signers"));
    }

    let mut infos = Vec::new();
    for signer in &signers {
        // Pick the best supported signature: highest priority = PSS/ECDSA.
        let supported: Vec<&(u32, Vec<u8>)> = signer
            .signatures
            .iter()
            .filter(|(id, _)| SigAlg::from_id(*id).is_some())
            .collect();
        if supported.is_empty() {
            return Err(sig_err("sig.alg.unsupported"));
        }
        let mut verified = false;
        let mut used_alg = 0;
        for (alg_id, sig) in supported {
            let alg = SigAlg::from_id(*alg_id).unwrap();
            // Spec: digests are keyed by signature algorithm ID.
            if !signer.digests.iter().any(|(d, _)| d == alg_id) {
                continue;
            }
            match verify_signature_bytes(alg, &signer.public_key, &signer.signed_data, sig) {
                Ok(()) => {
                    verified = true;
                    used_alg = *alg_id;
                    break;
                }
                Err(_) => continue,
            }
        }
        if !verified {
            return Err(sig_err("sig.verify.failed"));
        }

        // publicKey must equal the key inside certificates[0].
        let cert0 = &signer.certs[0];
        let (_, parsed) = x509_parser::prelude::X509Certificate::from_der(cert0)
            .map_err(|_| sig_err("sig.cert.malformed"))?;
        let spki = parsed.tbs_certificate.subject_pki.raw;
        if spki != signer.public_key.as_slice() {
            return Err(sig_err("sig.key.mismatch"));
        }

        // Content digest must match for the digest alg of the used signature.
        let alg = SigAlg::from_id(used_alg).unwrap();
        let digest_alg = alg.digest_alg();
        let expected = signer
            .digests
            .iter()
            .find(|(d, _)| *d == used_alg)
            .ok_or_else(|| sig_err("sig.digest.absent"))?
            .1
            .clone();
        let actual = content_digest(bytes, &cd, block_start, digest_alg)?;
        if actual != expected {
            return Err(sig_err("sig.digest.mismatch").with_detail(format!(
                "expected={} actual={} block_start={} cd={}+{}",
                expected
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
                actual
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
                block_start,
                cd.offset,
                cd.size
            )));
        }

        let subject = parsed
            .tbs_certificate
            .subject
            .iter_common_name()
            .next()
            .and_then(|cn| cn.as_str().ok().map(|s| s.to_string()))
            .unwrap_or_else(|| parsed.tbs_certificate.subject.to_string());
        let fp: String = sha256(cert0).iter().map(|b| format!("{b:02x}")).collect();
        infos.push(SignerInfo {
            cert_sha256: fp,
            sig_alg: used_alg,
            subject,
        });
    }
    Ok(infos)
}

// ---- v1 (JAR) signature verification ----
//
// Chain of trust: the PKCS#7 block (META-INF/*.RSA) signs the .SF file,
// .SF digests cover MANIFEST.MF sections, and per-entry digests in the
// manifest cover each archive member.

/// DER TLV reader — returns (tag, content_start, tlv_end).
fn der_tlv(b: &[u8], pos: usize) -> Result<(u8, usize, usize)> {
    if pos + 2 > b.len() {
        return Err(sig_err("sig.v1.der.truncated"));
    }
    let tag = b[pos];
    if tag & 0x1f == 0x1f {
        // Multi-byte tags never appear in PKCS#7/X.509 — refuse.
        return Err(sig_err("sig.v1.der.tag"));
    }
    let mut p = pos + 1;
    let len0 = b[p] as usize;
    p += 1;
    let len = if len0 & 0x80 == 0 {
        len0
    } else {
        let n = len0 & 0x7f;
        if n == 0 || n > 4 || p + n > b.len() {
            return Err(sig_err("sig.v1.der.len"));
        }
        let mut l = 0usize;
        for i in 0..n {
            l = (l << 8) | b[p + i] as usize;
        }
        p += n;
        l
    };
    let end = p
        .checked_add(len)
        .filter(|e| *e <= b.len())
        .ok_or_else(|| sig_err("sig.v1.der.bounds"))?;
    Ok((tag, p, end))
}

/// Child elements of a constructed DER value: (tag, tlv_start, content_start, tlv_end).
fn der_children(b: &[u8], start: usize, end: usize) -> Result<Vec<(u8, usize, usize, usize)>> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos < end {
        let (tag, cs, te) = der_tlv(b, pos)?;
        out.push((tag, pos, cs, te));
        pos = te;
    }
    Ok(out)
}

/// OID bytes of an AlgorithmIdentifier/ContentInfo OID element.
fn der_oid(b: &[u8], start: usize, end: usize) -> Result<Vec<u8>> {
    let kids = der_children(b, start, end)?;
    for (tag, _ts, cs, te) in &kids {
        if *tag == 0x06 {
            return Ok(b[*cs..*te].to_vec());
        }
    }
    Err(sig_err("sig.v1.oid.missing"))
}

const OID_SIGNED_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02];
const OID_SHA1: &[u8] = &[0x2b, 0x0e, 0x03, 0x02, 0x1a];
const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
const OID_SHA512: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03];
const OID_SHA384: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02];
const OID_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_RSA_SHA1: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x05];
const OID_RSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
const OID_RSA_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];
const OID_RSA_SHA512: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d];
const OID_MESSAGE_DIGEST: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x04];

fn sha384(b: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha384::digest(b).to_vec()
}

/// Hash `data` with the digest named by `oid` (SHA-1/256/384/512).
fn digest_for_oid(oid: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    if oid == OID_SHA256 {
        Some(sha256(data))
    } else if oid == OID_SHA384 {
        Some(sha384(data))
    } else if oid == OID_SHA512 {
        Some(sha512(data))
    } else if oid == OID_SHA1 {
        use sha1::Sha1;
        Some(Sha1::digest(data).to_vec())
    } else {
        None
    }
}

/// Hash `data` with the digest named by a manifest header
/// ("SHA-256-Digest", "SHA1-Digest-Manifest", …).
fn digest_for_name(k: &str, data: &[u8]) -> Option<Vec<u8>> {
    if k.starts_with("SHA-256") {
        Some(sha256(data))
    } else if k.starts_with("SHA-384") {
        Some(sha384(data))
    } else if k.starts_with("SHA-512") {
        Some(sha512(data))
    } else if k.starts_with("SHA1") || k.starts_with("SHA-1") {
        use sha1::Sha1;
        Some(Sha1::digest(data).to_vec())
    } else {
        None
    }
}

/// Split a manifest/.SF body into raw sections, each ending at (and
/// including) the blank line that terminates it. A blank line is two
/// consecutive line terminators (\r\n | \n | \r).
fn jar_sections(b: &[u8]) -> Vec<(usize, usize)> {
    let term = |b: &[u8], i: usize| -> usize {
        if i + 1 < b.len() && b[i] == b'\r' && b[i + 1] == b'\n' {
            2
        } else if i < b.len() && (b[i] == b'\n' || b[i] == b'\r') {
            1
        } else {
            0
        }
    };
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        let t1 = term(b, i);
        if t1 == 0 {
            i += 1;
            continue;
        }
        let j = i + t1;
        let t2 = term(b, j);
        if t2 > 0 {
            let end = j + t2;
            out.push((start, end));
            start = end;
            i = end;
        } else {
            i = j;
        }
    }
    if start < b.len() {
        out.push((start, b.len()));
    }
    out
}

/// Parse `Key: value` headers (with continuation lines starting ' ') of one
/// manifest-style section.
fn jar_headers(b: &[u8]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in b.split(|&c| c == b'\n') {
        let line = std::str::from_utf8(line).unwrap_or("");
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if let Some(cont) = line.strip_prefix(' ') {
            if let Some((_, v)) = out.last_mut() {
                v.push_str(cont);
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(": ") {
            out.push((k.to_string(), v.to_string()));
        }
    }
    out
}

fn b64(s: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|_| sig_err("sig.v1.b64"))
}

/// Verify one JAR signer: PKCS#7 block signs `sf`, .SF digests cover
/// `manifest`, manifest digests cover every non-META-INF member.
fn verify_v1_signer(
    bytes: &[u8],
    entries: &[crate::zip::CentralEntry],
    manifest: &[u8],
    sf_e: &crate::zip::CentralEntry,
) -> Result<SignerInfo> {
    use crate::zip::read_entry;
    let sf = read_entry(bytes, sf_e)?;
    let stem = &sf_e.name[..sf_e.name.len().saturating_sub(3)];
    let block_e = entries
        .iter()
        .find(|e| {
            let n = e.name.to_ascii_uppercase();
            let stem_up = stem.to_ascii_uppercase();
            n == format!("{stem_up}.RSA")
                || n == format!("{stem_up}.DSA")
                || n == format!("{stem_up}.EC")
        })
        .ok_or_else(|| sig_err("sig.v1.block.missing"))?;
    let block = read_entry(bytes, block_e)?;

    // --- PKCS#7: ContentInfo → [0] → SignedData ---
    let (t, cs, ce) = der_tlv(&block, 0)?;
    if t != 0x30 {
        return Err(sig_err("sig.v1.pkcs7"));
    }
    let outer = der_children(&block, cs, ce)?;
    // ContentInfo's first child is the content-type OID itself.
    let oid = match outer.first() {
        Some(&(0x06, _ts, os, oe)) => block[os..oe].to_vec(),
        _ => return Err(sig_err("sig.v1.oid.missing")),
    };
    if oid != OID_SIGNED_DATA {
        return Err(sig_err("sig.v1.not_signed_data"));
    }
    let sd = outer
        .iter()
        .find(|x| x.0 == 0xa0)
        .ok_or_else(|| sig_err("sig.v1.content.missing"))?;
    let (t, sds, sde) = der_tlv(&block, sd.2)?;
    if t != 0x30 {
        return Err(sig_err("sig.v1.pkcs7"));
    }
    let sd_children = der_children(&block, sds, sde)?;
    let certs_ctx = sd_children.iter().find(|x| x.0 == 0xa0);
    // Two SETs appear in SignedData — digestAlgorithms first, signerInfos
    // last. Take the final one.
    let signer_infos_set = sd_children
        .iter()
        .rev()
        .find(|x| x.0 == 0x31)
        .ok_or_else(|| sig_err("sig.v1.signerinfo.missing"))?;
    let cert_ctx = certs_ctx.ok_or_else(|| sig_err("sig.v1.certs.missing"))?;
    // Every certificate inside the [0] context — when a chain is present
    // the signer's leaf may not be first, so verification tries each.
    let cert_ders: Vec<(usize, usize)> = der_children(&block, cert_ctx.2, cert_ctx.3)?
        .iter()
        .filter(|x| x.0 == 0x30)
        .map(|x| (x.1, x.3))
        .collect();
    if cert_ders.is_empty() {
        return Err(sig_err("sig.v1.cert"));
    }

    // --- SignerInfo: version, sid, digestAlg, [0] attrs?, sigAlg, signature ---
    let si_children = der_children(&block, signer_infos_set.2, signer_infos_set.3)?;
    let (t, sis, sie) = si_children
        .first()
        .map(|x| der_tlv(&block, x.1))
        .ok_or_else(|| sig_err("sig.v1.signerinfo"))??;
    if t != 0x30 {
        return Err(sig_err("sig.v1.signerinfo"));
    }
    let fields = der_children(&block, sis, sie)?;
    // version, sid, digestAlgorithm, signatureAlgorithm and the signature
    // value are all mandatory — a truncated SignerInfo is an error, not a
    // panic.
    if fields.len() < 5 {
        return Err(sig_err("sig.v1.signerinfo"));
    }
    let digest_oid = der_oid(&block, fields[2].2, fields[2].3)?;
    let (signed_attrs, sig_alg_i, sig_i) = if fields[3].0 == 0xa0 {
        (Some(&fields[3]), 4usize, 5usize)
    } else {
        (None, 3usize, 4usize)
    };
    if fields.len() <= sig_i {
        return Err(sig_err("sig.v1.signerinfo"));
    }
    let sig_alg_oid = der_oid(&block, fields[sig_alg_i].2, fields[sig_alg_i].3)?;
    let signature = {
        let (t, ss, se) = der_tlv(&block, fields[sig_i].1)?;
        if t != 0x04 {
            return Err(sig_err("sig.v1.signature"));
        }
        &block[ss..se]
    };

    // Signed message: with signedAttrs the signature covers the DER SET OF
    // attributes (tag swapped 0xa0 → 0x31); otherwise the raw .SF bytes.
    let msg: Vec<u8> = match signed_attrs {
        Some(f) => {
            let mut m = block[f.1..f.3].to_vec();
            m[0] = 0x31;
            m
        }
        None => sf.clone(),
    };
    // With signedAttrs the signature alone proves nothing about the .SF:
    // PKCS#7 requires a messageDigest attribute carrying the content
    // digest — verify it covers the .SF bytes actually on disk, or an
    // attacker could swap the .SF and keep the signature valid.
    if let Some(f) = signed_attrs {
        let mut want: Option<Vec<u8>> = None;
        for a in der_children(&block, f.2, f.3)? {
            if a.0 != 0x30 {
                continue;
            }
            let parts = der_children(&block, a.2, a.3)?;
            let Some(&(0x06, _ts, os, oe)) = parts.first() else {
                continue;
            };
            if block[os..oe] != *OID_MESSAGE_DIGEST {
                continue;
            }
            let Some(&(0x31, _ts, vs, ve)) = parts.get(1) else {
                continue;
            };
            if let Some(&(0x04, _ts, cs, ce)) = der_children(&block, vs, ve)?.first() {
                want = Some(block[cs..ce].to_vec());
            }
        }
        let Some(want) = want else {
            return Err(sig_err("sig.v1.md.missing"));
        };
        let got = digest_for_oid(&digest_oid, &sf).ok_or_else(|| sig_err("sig.v1.digest_alg"))?;
        if got != want {
            return Err(sig_err("sig.v1.md.mismatch"));
        }
    }
    use rsa::pkcs8::DecodePublicKey;
    // Digest alg inside SignerInfo and the signature alg must agree.
    let dig_ok = [OID_SHA256, OID_SHA384, OID_SHA1, OID_SHA512].contains(&digest_oid.as_slice());
    if !dig_ok {
        return Err(sig_err("sig.v1.digest_alg"));
    }
    // signatureAlgorithm is either rsaEncryption (digest picked from
    // digestAlgorithm, per PKCS#1) or the explicit hashWithRSA OID.
    let rsa_ok = sig_alg_oid == OID_RSA
        || (sig_alg_oid == OID_RSA_SHA256 && digest_oid == OID_SHA256)
        || (sig_alg_oid == OID_RSA_SHA384 && digest_oid == OID_SHA384)
        || (sig_alg_oid == OID_RSA_SHA1 && digest_oid == OID_SHA1)
        || (sig_alg_oid == OID_RSA_SHA512 && digest_oid == OID_SHA512);
    if !rsa_ok {
        return Err(sig_err("sig.v1.alg.unsupported"));
    }
    let verify_with = |spki: &[u8]| -> bool {
        let Ok(key) = rsa::RsaPublicKey::from_public_key_der(spki) else {
            return false;
        };
        if digest_oid == OID_SHA256 {
            key.verify(rsa::Pkcs1v15Sign::new::<Sha256>(), &sha256(&msg), signature)
                .is_ok()
        } else if digest_oid == OID_SHA384 {
            key.verify(
                rsa::Pkcs1v15Sign::new::<sha2::Sha384>(),
                &sha384(&msg),
                signature,
            )
            .is_ok()
        } else if digest_oid == OID_SHA1 {
            use sha1::Sha1;
            key.verify(
                rsa::Pkcs1v15Sign::new::<Sha1>(),
                &Sha1::digest(&msg),
                signature,
            )
            .is_ok()
        } else {
            digest_oid == OID_SHA512
                && key
                    .verify(rsa::Pkcs1v15Sign::new::<Sha512>(), &sha512(&msg), signature)
                    .is_ok()
        }
    };
    // Try every certificate in the block — the signer's leaf may sit
    // behind CA certs when a chain is embedded.
    let cert_i = cert_ders
        .iter()
        .enumerate()
        .find_map(|(i, &(cs, ce))| {
            let (_, parsed) =
                x509_parser::prelude::X509Certificate::from_der(&block[cs..ce]).ok()?;
            verify_with(parsed.tbs_certificate.subject_pki.raw).then_some(i)
        })
        .ok_or_else(|| sig_err("sig.v1.verify.failed"))?;
    let (cert_cs, cert_ce) = cert_ders[cert_i];
    let cert_der = &block[cert_cs..cert_ce];
    let (_, parsed) = x509_parser::prelude::X509Certificate::from_der(cert_der)
        .map_err(|_| sig_err("sig.v1.cert.malformed"))?;

    // --- .SF → MANIFEST.MF digest chain ---
    let sf_sections = jar_sections(&sf);
    let main = sf_sections
        .first()
        .map(|(s, e)| jar_headers(&sf[*s..*e]))
        .unwrap_or_default();
    let whole_digests: Vec<(String, Vec<u8>)> = main
        .iter()
        .filter(|(k, _)| k.ends_with("-Digest-Manifest"))
        .map(|(k, v)| Ok((k.clone(), b64(v)?)))
        .collect::<Result<Vec<_>>>()?;
    let mut manifest_ok = false;
    for (k, want) in &whole_digests {
        let Some(got) = digest_for_name(k, manifest) else {
            continue;
        };
        if &got == want {
            manifest_ok = true;
            break;
        }
    }
    if !manifest_ok && !whole_digests.is_empty() {
        return Err(sig_err("sig.v1.manifest.mismatch"));
    }
    if whole_digests.is_empty() {
        // Per-section digests: each `Name: <sec>` maps to the manifest
        // section whose `Name:` header matches; the digested bytes are the
        // manifest section verbatim (blank line included).
        let mf_sections = jar_sections(manifest);
        let mut checked = 0usize;
        for (s, e) in &sf_sections {
            let hdrs = jar_headers(&sf[*s..*e]);
            let Some(name) = hdrs
                .iter()
                .find(|(k, _)| k == "Name")
                .map(|(_, v)| v.clone())
            else {
                continue;
            };
            let Some(digest) = hdrs
                .iter()
                .find(|(k, _)| k.ends_with("-Digest"))
                .map(|(k, v)| (k.clone(), v.clone()))
            else {
                continue;
            };
            let want = b64(&digest.1)?;
            let mf_sec = mf_sections.iter().find(|(s, e)| {
                jar_headers(&manifest[*s..*e])
                    .iter()
                    .any(|(k, v)| k == "Name" && *v == name)
            });
            let Some(&(ms, me)) = mf_sec else {
                return Err(sig_err("sig.v1.section.missing"));
            };
            // JAR signers differ on whether the terminating blank line is
            // inside the digested span — accept either byte range.
            let span_full = &manifest[ms..me];
            let span_trim = {
                let mut e2 = me;
                while e2 > ms && (manifest[e2 - 1] == b'\n' || manifest[e2 - 1] == b'\r') {
                    e2 -= 1;
                }
                &manifest[ms..e2]
            };
            let ok = [span_full, span_trim]
                .iter()
                .any(|sp| digest_for_name(&digest.0, sp).is_some_and(|g| g == want));
            if !ok {
                return Err(sig_err("sig.v1.section.mismatch"));
            }
            checked += 1;
        }
        if checked == 0 {
            return Err(sig_err("sig.v1.sf.nothing"));
        }
        // Coverage runs both ways: every manifest section must also be
        // authenticated by an .SF section, or entries added to the
        // manifest after signing would slip through unsigned.
        let sf_names: std::collections::HashSet<String> = sf_sections
            .iter()
            .flat_map(|(s, e)| jar_headers(&sf[*s..*e]))
            .filter(|(k, _)| k == "Name")
            .map(|(_, v)| v)
            .collect();
        for (s, e) in &mf_sections {
            let has_name = jar_headers(&manifest[*s..*e])
                .iter()
                .any(|(k, v)| k == "Name" && sf_names.contains(v));
            let is_main = !jar_headers(&manifest[*s..*e])
                .iter()
                .any(|(k, _)| k == "Name");
            if !is_main && !has_name {
                return Err(sig_err("sig.v1.section.uncovered"));
            }
        }
    }

    // --- Manifest per-entry digests → zip members ---
    let mut files_checked = 0usize;
    for (s, e) in jar_sections(manifest) {
        let hdrs = jar_headers(&manifest[s..e]);
        let Some(name) = hdrs
            .iter()
            .find(|(k, _)| k == "Name")
            .map(|(_, v)| v.clone())
        else {
            continue;
        };
        let entry = entries.iter().find(|en| en.name == name);
        let Some(entry) = entry else {
            return Err(sig_err("sig.v1.entry.missing"));
        };
        for (k, v) in hdrs.iter().filter(|(k, _)| k.ends_with("-Digest")) {
            let want = b64(v)?;
            let data = read_entry(bytes, entry)?;
            let Some(got) = digest_for_name(k, &data) else {
                continue;
            };
            if got != want {
                return Err(sig_err("sig.v1.entry.mismatch"));
            }
            files_checked += 1;
        }
    }
    // Every non-signature member must appear in the manifest.
    let is_sig_meta = |n: &str| {
        let u = n.to_ascii_uppercase();
        u == "META-INF/MANIFEST.MF"
            || (u.starts_with("META-INF/")
                && (u.ends_with(".SF")
                    || u.ends_with(".RSA")
                    || u.ends_with(".DSA")
                    || u.ends_with(".EC")))
    };
    let listed: std::collections::HashSet<String> = jar_sections(manifest)
        .iter()
        .flat_map(|(s, e)| jar_headers(&manifest[*s..*e]))
        .filter(|(k, _)| k == "Name")
        .map(|(_, v)| v)
        .collect();
    for en in entries {
        if en.name.ends_with('/') || is_sig_meta(&en.name) {
            continue;
        }
        if !listed.contains(&en.name) {
            return Err(sig_err("sig.v1.entry.unlisted"));
        }
    }
    if files_checked == 0 {
        return Err(sig_err("sig.v1.manifest.nodigests"));
    }

    let subject = parsed
        .tbs_certificate
        .subject
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok().map(|s| s.to_string()))
        .unwrap_or_else(|| parsed.tbs_certificate.subject.to_string());
    Ok(SignerInfo {
        cert_sha256: sha256(cert_der)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        sig_alg: 0,
        subject,
    })
}

/// Verify an APK carrying only a v1 (JAR) signature — the fallback when no
/// v2 signing block exists. Errors are `PkgSignature`.
pub fn verify_v1(bytes: &[u8]) -> Result<Vec<SignerInfo>> {
    use crate::zip::{parse_central_directory, read_entry, CentralEntry, ZipLimits};
    let (_cd, entries) = parse_central_directory(bytes, &ZipLimits::default())?;
    let is_meta = |e: &&CentralEntry| e.name.to_ascii_uppercase().starts_with("META-INF/");
    let manifest_e = entries
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case("META-INF/MANIFEST.MF"))
        .ok_or_else(|| sig_err("sig.v1.manifest.missing"))?;
    let manifest = read_entry(bytes, manifest_e)?;
    let sf_entries: Vec<&CentralEntry> = entries
        .iter()
        .filter(|e| {
            is_meta(e) && e.name.to_ascii_uppercase().ends_with(".SF") && !e.name.ends_with('/')
        })
        .collect();
    if sf_entries.is_empty() {
        return Err(sig_err("sig.v1.sf.missing"));
    }
    // An .SF whose main section declares `X-Android-APK-Signed: 2` was
    // produced alongside a v2 signing block. Reaching v1 verification
    // means that block was stripped — reject the downgrade.
    for sf_e in &sf_entries {
        let sf = read_entry(bytes, sf_e)?;
        if let Some(&(s, e)) = jar_sections(&sf).first() {
            for (k, v) in jar_headers(&sf[s..e]) {
                if k.eq_ignore_ascii_case("X-Android-APK-Signed")
                    && v.split(',').any(|t| t.trim() == "2")
                {
                    return Err(sig_err("sig.v1.downgrade"));
                }
            }
        }
    }
    let mut infos = Vec::new();
    for sf_e in sf_entries {
        infos.push(verify_v1_signer(bytes, &entries, &manifest, sf_e)?);
    }
    Ok(infos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip::parse_central_directory;
    use crate::zip::ZipLimits;

    fn lp(body: &[u8]) -> Vec<u8> {
        let mut v = (body.len() as u32).to_le_bytes().to_vec();
        v.extend(body);
        v
    }

    fn build_zip_stored(name: &str, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let crc = crc32fast::hash(data);
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(20u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(crc.to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(name.as_bytes());
        out.extend(data);
        let cd_off = out.len() as u32;
        out.extend(0x0201_4b50u32.to_le_bytes());
        out.extend(20u16.to_le_bytes());
        out.extend(20u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(crc.to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(name.as_bytes());
        let cd_size = 46 + name.len() as u32;
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(cd_size.to_le_bytes());
        out.extend(cd_off.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    /// Wrap content sections + insert signing block before CD, fix EOCD cd_offset.
    fn wrap_with_block(zip: Vec<u8>, block_pairs: Vec<u8>) -> Vec<u8> {
        let cd = find_central_directory(&zip).unwrap();
        let cd_off = cd.offset as usize;
        let mut out = zip[..cd_off].to_vec();
        out.extend(&block_pairs);
        out.extend(&zip[cd_off..]);
        // Fix EOCD cd-offset: EOCD is at the end, cd_offset at eocd+16.
        let eocd = out.len() - 22;
        let new_cd = (cd_off + block_pairs.len()) as u32;
        out[eocd + 16..eocd + 20].copy_from_slice(&new_cd.to_le_bytes());
        out
    }

    fn signing_block(pairs: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (id, val) in pairs {
            let len = 4 + val.len();
            body.extend((len as u64).to_le_bytes());
            body.extend(id.to_le_bytes());
            body.extend(val);
        }
        let size = body.len() + 16 + 8; // magic + trailing size field
        let mut block = (size as u64).to_le_bytes().to_vec();
        block.extend(&body);
        block.extend((size as u64).to_le_bytes());
        block.extend(SIG_BLOCK_MAGIC);
        block
    }

    /// Build a signed test APK using ECDSA P-256 (rcgen self-signed cert).
    fn build_signed_apk() -> (Vec<u8>, String) {
        let certified = rcgen::generate_simple_self_signed(vec!["Ovelo Test".to_string()]).unwrap();
        let cert_der = certified.cert.der().to_vec();
        // SPKI bytes exactly as the verifier reads them from the cert.
        let (_, parsed) = x509_parser::prelude::X509Certificate::from_der(&cert_der).unwrap();
        let pubkey_der = parsed.tbs_certificate.subject_pki.raw.to_vec();

        let zip = build_zip_stored("classes.dex", b"fake dex bytes");
        let cd = find_central_directory(&zip).unwrap();

        // Compute digest at block start position (= cd.offset).
        let block_start = cd.offset as usize;
        let digest = content_digest(&zip, &cd, block_start, 1).unwrap();

        // digest record = u32 recLen || u32 algId || u32 digestLen || digest.
        let mut body = 0x0201u32.to_le_bytes().to_vec();
        body.extend(&(digest.len() as u32).to_le_bytes());
        body.extend(&digest);
        let mut digest_rec = (body.len() as u32).to_le_bytes().to_vec();
        digest_rec.extend(&body);
        let digests_seq = digest_rec.clone();
        let certs_seq = lp(&cert_der);
        let attrs_seq: Vec<u8> = Vec::new();
        let signed_data = [lp(&digests_seq), lp(&certs_seq), lp(&attrs_seq)].concat();

        use p256::ecdsa::{signature::Signer, Signature, SigningKey};
        use p256::pkcs8::DecodePrivateKey;
        let sk = SigningKey::from_pkcs8_der(&certified.key_pair.serialize_der()).unwrap();
        let sig: Signature = sk.sign(&signed_data);
        let sig_der = sig.to_der().as_bytes().to_vec();
        // signature record = u32 recLen || u32 algId || u32 sigLen || sig.
        let mut body = 0x0201u32.to_le_bytes().to_vec();
        body.extend(&(sig_der.len() as u32).to_le_bytes());
        body.extend(&sig_der);
        let mut sig_rec = (body.len() as u32).to_le_bytes().to_vec();
        sig_rec.extend(&body);

        let signer = [lp(&signed_data), lp(&sig_rec), lp(&pubkey_der)].concat();
        let signers = lp(&lp(&signer));
        let block = signing_block(&[(ID_V2, signers)]);
        let apk = wrap_with_block(zip, block);
        (apk, hex_upper(&sha256(&cert_der)))
    }

    fn hex_upper(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn verifies_valid_v2_apk() {
        let (apk, fp) = build_signed_apk();
        let infos = verify_v2(&apk).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].cert_sha256, fp);
        assert_eq!(infos[0].sig_alg, 0x0201);
        assert!(!infos[0].subject.is_empty());
        // Zip still parses.
        let (_cd, entries) = parse_central_directory(&apk, &ZipLimits::default()).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn rejects_tampered_content() {
        let (mut apk, _) = build_signed_apk();
        apk[40] ^= 0xff; // inside zip data section
        let e = verify_v2(&apk).unwrap_err();
        assert_eq!(e.category(), ErrorCategory::PkgSignature);
    }

    #[test]
    fn rejects_missing_block() {
        let zip = build_zip_stored("a", b"b");
        assert!(verify_v2(&zip).is_err());
    }
}
