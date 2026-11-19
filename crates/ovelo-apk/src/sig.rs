//! APK Signature Scheme v2 verification (design doc §03 step 3).
//!
//! Layout: `[entries][APK Signing Block][central directory][EOCD]`.
//! The signing block: `u64 size | pairs | u64 size | "APK Sig Block 42"`.
//! v2 pair id = 0x7109871a, v3 = 0xf05368c0.

use sha2::{Digest, Sha256, Sha512};
use ovelo_error::{ErrorCategory, OveloError, Phase, Result};
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
        if len < 4 || pos + 8 + len > end {
            return Err(sig_err("sig.pair.bounds"));
        }
        let id = u32::from_le_bytes(bytes[pos + 8..pos + 12].try_into().unwrap());
        out.push((id, &bytes[pos + 12..pos + 8 + len]));
        pos += 8 + len;
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
                key.verify(rsa::Pkcs1v15Sign::new::<Sha256>(), &sha256(signed_data), signature)
            } else {
                key.verify(rsa::Pkcs1v15Sign::new::<Sha512>(), &sha512(signed_data), signature)
            };
            res.map_err(|_| sig_err("sig.verify.failed"))
        }
        SigAlg::EcdsaSha256 => {
            use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
            let key = VerifyingKey::from_public_key_der(public_key_der)
                .map_err(|_| sig_err("sig.key.ec"))?;
            let sig = Signature::from_der(signature)
                .map_err(|_| sig_err("sig.signature.malformed"))?;
            key.verify(signed_data, &sig)
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

        // signatures: sequence of length-prefixed {algId, sig}
        let mut sig_list = Vec::new();
        let mut sig_seq = Lp::new(signatures);
        while !sig_seq.done() {
            let rec = sig_seq.item()?;
            if rec.len() < 4 {
                return Err(sig_err("sig.record.truncated"));
            }
            let alg = u32::from_le_bytes(rec[..4].try_into().unwrap());
            sig_list.push((alg, rec[4..].to_vec()));
        }

        // signedData: lp(digests) || lp(certs) || lp(attrs)
        let mut sd = Lp::new(&signed_data);
        let digests_seq = sd.item()?;
        let certs_seq = sd.item()?;
        let _attrs = sd.item().unwrap_or(&[]);
        // digests: sequence of (uint32 sigAlgId, fixed-length digest) records —
        // the digest length is implied by the algorithm.
        let mut digests = Vec::new();
        let mut dp = 0usize;
        while dp + 4 <= digests_seq.len() {
            let alg = u32::from_le_bytes(digests_seq[dp..dp + 4].try_into().unwrap());
            let dlen = match SigAlg::from_id(alg) {
                Some(a) => match a.digest_alg() {
                    1 => 32,
                    2 => 64,
                    _ => return Err(sig_err("sig.digest_alg.unsupported")),
                },
                None => return Err(sig_err("sig.alg.unsupported")),
            };
            dp += 4;
            if dp + dlen > digests_seq.len() {
                return Err(sig_err("sig.record.truncated"));
            }
            digests.push((alg, digests_seq[dp..dp + dlen].to_vec()));
            dp += dlen;
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
                expected.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                actual.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                block_start, cd.offset, cd.size
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip::ZipLimits;
    use crate::zip::parse_central_directory;

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
        let certified =
            rcgen::generate_simple_self_signed(vec!["Ovelo Test".to_string()]).unwrap();
        let cert_der = certified.cert.der().to_vec();
        // SPKI bytes exactly as the verifier reads them from the cert.
        let (_, parsed) =
            x509_parser::prelude::X509Certificate::from_der(&cert_der).unwrap();
        let pubkey_der = parsed.tbs_certificate.subject_pki.raw.to_vec();

        let zip = build_zip_stored("classes.dex", b"fake dex bytes");
        let cd = find_central_directory(&zip).unwrap();

        // Compute digest at block start position (= cd.offset).
        let block_start = cd.offset as usize;
        let digest = content_digest(&zip, &cd, block_start, 1).unwrap();

        let mut digest_rec = 0x0201u32.to_le_bytes().to_vec(); // alg id
        digest_rec.extend(&digest);
        // digests field = raw (algId u32, digest) records; certs field = lp'd certs.
        let digests_seq = digest_rec.clone();
        let certs_seq = lp(&cert_der);
        let attrs_seq: Vec<u8> = Vec::new();
        let signed_data = [
            lp(&digests_seq),
            lp(&certs_seq),
            lp(&attrs_seq),
        ]
        .concat();

        use p256::ecdsa::{signature::Signer, Signature, SigningKey};
        use p256::pkcs8::DecodePrivateKey;
        let sk = SigningKey::from_pkcs8_der(&certified.key_pair.serialize_der()).unwrap();
        let sig: Signature = sk.sign(&signed_data);
        let sig_der = sig.to_der().as_bytes().to_vec();
        let mut sig_rec = 0x0201u32.to_le_bytes().to_vec();
        sig_rec.extend(&sig_der);

        let signer = [lp(&signed_data), lp(&lp(&sig_rec)), lp(&pubkey_der)].concat();
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
        let (_cd, entries) =
            parse_central_directory(&apk, &ZipLimits::default()).unwrap();
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
