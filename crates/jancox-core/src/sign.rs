//! Signatures for repacked OTA zips.
//!
//! - payload.bin: RSA PKCS#1 v1.5 over SHA-256, as `payload_signer.cc`.
//! - The whole zip, as `signapk -w`: a detached PKCS#7 SignedData (no
//!   signed attributes) over everything up to the zip comment length,
//!   stored in the zip comment with a 6-byte footer. Recovery's
//!   `verify_file` checks it; the hash is SHA-1 or SHA-256 as the
//!   certificate's own signature algorithm says.
//!
//! The default key is the AOSP test key (`keys/`), which test-keys builds
//! such as TWRP and OrangeFox trust.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};
use rsa::{Pkcs1v15Sign, RsaPrivateKey, RsaPublicKey};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::fs::invalid;

const TEST_KEY: &[u8] = include_bytes!("../keys/testkey.pk8");
const TEST_CERT: &[u8] = include_bytes!("../keys/testkey.x509.pem");

/// DigestInfo prefixes (RFC 8017 9.2).
const SHA1_PREFIX: &[u8] = &[
    0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14,
];
const SHA256_PREFIX: &[u8] = &[
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

// DER of the object identifiers used here
const OID_SHA1: &[u8] = &[0x2b, 0x0e, 0x03, 0x02, 0x1a];
const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
const OID_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_MD5_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x04];
const OID_SHA1_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x05];
const OID_PKCS7_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x01];
const OID_PKCS7_SIGNED: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02];

/// A private key and its certificate.
pub struct Key {
    key: RsaPrivateKey,
    /// X.509 certificate, DER.
    cert: Vec<u8>,
    /// The certificate is signed with SHA-1 (or MD5): recovery then checks
    /// the whole-file signature with SHA-1.
    zip_sha1: bool,
}

impl Key {
    /// The AOSP test key.
    pub fn test_key() -> io::Result<Key> {
        Key::from_bytes(TEST_KEY, TEST_CERT)
    }

    /// A PKCS#8 private key (DER, as `*.pk8`) and its X.509 certificate
    /// (PEM or DER, as `*.x509.pem`).
    pub fn load(pk8: &Path, cert: &Path) -> io::Result<Key> {
        let read = |p: &Path| {
            std::fs::read(p)
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", p.display(), e)))
        };
        Key::from_bytes(&read(pk8)?, &read(cert)?)
    }

    pub fn from_bytes(pk8: &[u8], cert: &[u8]) -> io::Result<Key> {
        let key = RsaPrivateKey::from_pkcs8_der(pk8)
            .map_err(|e| invalid(format!("private key: {} (want an RSA .pk8)", e)))?;
        let cert = if cert.starts_with(b"-----BEGIN") {
            pem_body(cert).ok_or_else(|| invalid("certificate: bad PEM"))?
        } else {
            cert.to_vec()
        };
        let tbs = Tbs::parse(&cert)?;
        let public = RsaPublicKey::from_public_key_der(tbs.spki)
            .map_err(|e| invalid(format!("certificate: {}", e)))?;
        if public != key.to_public_key() {
            return Err(invalid("the certificate does not match the private key"));
        }
        let zip_sha1 = matches!(tbs.sig_alg, OID_SHA1_RSA | OID_MD5_RSA);
        Ok(Key {
            key,
            cert,
            zip_sha1,
        })
    }

    /// Bytes of a signature (the key size).
    pub fn signature_len(&self) -> usize {
        rsa::traits::PublicKeyParts::size(&self.key)
    }

    fn sign(&self, prefix: &[u8], digest: &[u8]) -> io::Result<Vec<u8>> {
        let mut info = prefix.to_vec();
        info.extend_from_slice(digest);
        self.key
            .sign(Pkcs1v15Sign::new_unprefixed(), &info)
            .map_err(|e| io::Error::other(format!("RSA signature: {}", e)))
    }

    /// Signs a SHA-256 digest.
    pub fn sign_sha256(&self, digest: &[u8]) -> io::Result<Vec<u8>> {
        self.sign(SHA256_PREFIX, digest)
    }

    /// The certificate as PEM (for `META-INF/com/android/otacert`).
    pub fn cert_pem(&self) -> String {
        let b64 = base64(&self.cert);
        let mut s = String::from("-----BEGIN CERTIFICATE-----\n");
        for line in b64.as_bytes().chunks(64) {
            s.push_str(std::str::from_utf8(line).unwrap());
            s.push('\n');
        }
        s.push_str("-----END CERTIFICATE-----\n");
        s
    }
}

/// DER: tag + length + content.
fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let n = content.len();
    if n < 0x80 {
        out.push(n as u8);
    } else {
        let bytes = n.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

fn alg_id(oid: &[u8]) -> Vec<u8> {
    seq(&[&tlv(0x06, oid), &[0x05, 0x00]])
}

/// One DER element: (tag, content, whole element, rest).
type Element<'a> = (u8, &'a [u8], &'a [u8], &'a [u8]);

fn element(b: &[u8]) -> io::Result<Element<'_>> {
    let bad = || invalid("certificate: bad DER");
    let tag = *b.first().ok_or_else(bad)?;
    let first = *b.get(1).ok_or_else(bad)?;
    let (len, head) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return Err(bad());
        }
        let bytes = b.get(2..2 + n).ok_or_else(bad)?;
        (
            bytes.iter().fold(0usize, |a, &x| a << 8 | x as usize),
            2 + n,
        )
    };
    let end = head.checked_add(len).ok_or_else(bad)?;
    let whole = b.get(..end).ok_or_else(bad)?;
    Ok((tag, &whole[head..], whole, &b[end..]))
}

/// The parts of a certificate we need.
struct Tbs<'a> {
    serial: &'a [u8],
    issuer: &'a [u8],
    spki: &'a [u8],
    sig_alg: &'a [u8],
}

impl<'a> Tbs<'a> {
    fn parse(cert: &'a [u8]) -> io::Result<Tbs<'a>> {
        let (_, cert, _, _) = element(cert)?;
        let (_, tbs, _, rest) = element(cert)?;
        // signatureAlgorithm of the certificate: SEQUENCE { OID, ... }
        let (_, alg, _, _) = element(rest)?;
        let (_, sig_alg, _, _) = element(alg)?;
        let (tag, _, _, mut rest) = element(tbs)?;
        if tag != 0xa0 {
            // no explicit version: that was the serial number
            rest = tbs;
        }
        let (_, _, serial, rest) = element(rest)?;
        let (_, _, _, rest) = element(rest)?; // signature
        let (_, _, issuer, rest) = element(rest)?;
        let (_, _, _, rest) = element(rest)?; // validity
        let (_, _, _, rest) = element(rest)?; // subject
        let (_, _, spki, _) = element(rest)?;
        Ok(Tbs {
            serial,
            issuer,
            spki,
            sig_alg,
        })
    }
}

/// The detached PKCS#7 SignedData that `signapk -w` puts in the comment.
fn pkcs7(key: &Key, signature: &[u8]) -> io::Result<Vec<u8>> {
    let tbs = Tbs::parse(&key.cert)?;
    let digest = alg_id(if key.zip_sha1 { OID_SHA1 } else { OID_SHA256 });
    let signer = seq(&[
        &tlv(0x02, &[1]),
        &seq(&[tbs.issuer, tbs.serial]),
        &digest,
        &alg_id(OID_RSA),
        &tlv(0x04, signature),
    ]);
    let signed_data = seq(&[
        &tlv(0x02, &[1]),
        &tlv(0x31, &digest),
        &seq(&[&tlv(0x06, OID_PKCS7_DATA)]),
        &tlv(0xa0, &key.cert),
        &tlv(0x31, &signer),
    ]);
    Ok(seq(&[
        &tlv(0x06, OID_PKCS7_SIGNED),
        &tlv(0xa0, &signed_data),
    ]))
}

/// Comment that precedes the signature, as SignApk writes it.
const COMMENT: &[u8] = b"signed by jancox\0";

/// Adds the whole-file signature to a finished zip without a comment.
pub fn sign_zip(path: &Path, key: &Key) -> io::Result<()> {
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    let len = f.metadata()?.len();
    let mut eocd = [0u8; 22];
    if len < 22 {
        return Err(invalid("zip too short"));
    }
    f.seek(SeekFrom::Start(len - 22))?;
    f.read_exact(&mut eocd)?;
    if eocd[..4] != [0x50, 0x4b, 0x05, 0x06] || eocd[20..] != [0, 0] {
        return Err(invalid("the zip already has a comment"));
    }
    // signed: everything but the comment length
    let signed = len - 2;
    f.seek(SeekFrom::Start(0))?;
    let mut r = (&mut f).take(signed);
    let signature = if key.zip_sha1 {
        let mut h = Sha1::new();
        hash_reader(&mut r, |b| h.update(b))?;
        key.sign(SHA1_PREFIX, &h.finalize())?
    } else {
        let mut h = Sha256::new();
        hash_reader(&mut r, |b| h.update(b))?;
        key.sign(SHA256_PREFIX, &h.finalize())?
    };
    let p7 = pkcs7(key, &signature)?;
    let sig_start = p7.len() + 6;
    let comment_len = COMMENT.len() + sig_start;
    if comment_len > 0xffff {
        return Err(invalid("signature too big for the zip comment"));
    }
    let mut comment = COMMENT.to_vec();
    comment.extend_from_slice(&p7);
    comment.extend_from_slice(&(sig_start as u16).to_le_bytes());
    comment.extend_from_slice(&[0xff, 0xff]);
    comment.extend_from_slice(&(comment_len as u16).to_le_bytes());
    // recovery refuses a comment that looks like an end of central directory
    if comment.windows(4).any(|w| w == [0x50, 0x4b, 0x05, 0x06]) {
        return Err(io::Error::other(
            "zip signature contains an EOCD marker; repack again",
        ));
    }
    f.seek(SeekFrom::Start(signed))?;
    f.write_all(&(comment_len as u16).to_le_bytes())?;
    f.write_all(&comment)?;
    f.flush()
}

/// Feeds everything `r` reads to `update`.
pub fn hash_reader(r: &mut impl Read, mut update: impl FnMut(&[u8])) -> io::Result<u64> {
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0;
    loop {
        match r.read(&mut buf)? {
            0 => return Ok(total),
            n => {
                update(&buf[..n]);
                total += n as u64;
            }
        }
    }
}

/// SHA-256 of a file.
pub fn sha256_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut h = Sha256::new();
    hash_reader(&mut File::open(path)?, |b| h.update(b))?;
    Ok(h.finalize().into())
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decodes base64, skipping whitespace.
pub fn from_base64(s: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for &c in s {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The DER inside the first PEM block.
fn pem_body(pem: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(pem).ok()?;
    let body: String = text
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    from_base64(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_zip() {
        use std::io::Cursor;
        let key = Key::test_key().unwrap();
        let path = std::env::temp_dir().join(format!("jancox-sign-{}.zip", std::process::id()));
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        zip.start_file("a.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"hello").unwrap();
        zip.finish().unwrap();
        let unsigned = std::fs::read(&path).unwrap();
        sign_zip(&path, &key).unwrap();
        let signed = std::fs::read(&path).unwrap();
        assert!(sign_zip(&path, &key).is_err());

        // footer: signature start, 0xffff, comment size
        let n = signed.len();
        let start = u16::from_le_bytes([signed[n - 6], signed[n - 5]]) as usize;
        let comment = u16::from_le_bytes([signed[n - 2], signed[n - 1]]) as usize;
        assert_eq!(&signed[n - 4..n - 2], &[0xff, 0xff]);
        assert_eq!(n, unsigned.len() + comment);
        assert_eq!(
            &signed[..unsigned.len() - 2],
            &unsigned[..unsigned.len() - 2]
        );
        // the PKCS#7 ends with the signature (OCTET STRING of 256 bytes)
        let p7 = &signed[n - start..n - 6];
        let sig = &p7[p7.len() - 256..];
        let digest = Sha1::digest(&signed[..unsigned.len() - 2]);
        let mut info = SHA1_PREFIX.to_vec();
        info.extend_from_slice(&digest);
        assert!(key
            .key
            .to_public_key()
            .verify(Pkcs1v15Sign::new_unprefixed(), &info, sig)
            .is_ok());
        let mut archive = zip::ZipArchive::new(Cursor::new(signed)).unwrap();
        assert_eq!(archive.by_name("a.txt").unwrap().size(), 5);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn base64_round_trip() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"foob", b"\xff\x00\xfe"] {
            assert_eq!(from_base64(base64(data).as_bytes()).unwrap(), data);
        }
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn test_key_signs() {
        let key = Key::test_key().unwrap();
        assert_eq!(key.signature_len(), 256);
        assert!(key.zip_sha1);
        let digest = Sha256::digest(b"jancox");
        let sig = key.sign_sha256(&digest).unwrap();
        let public = key.key.to_public_key();
        let mut info = SHA256_PREFIX.to_vec();
        info.extend_from_slice(&digest);
        assert!(public
            .verify(Pkcs1v15Sign::new_unprefixed(), &info, &sig)
            .is_ok());
        assert!(key
            .cert_pem()
            .starts_with("-----BEGIN CERTIFICATE-----\nMII"));
        let other = Key::from_bytes(TEST_KEY, key.cert_pem().as_bytes()).unwrap();
        assert_eq!(other.cert, key.cert);
    }
}
