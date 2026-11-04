// SPDX-License-Identifier: AGPL-3.0-only
//! The only crypto jsonwebtoken gets: RS256 verification on `ring` (already linked through
//! rustls). jsonwebtoken still parses and validates the token; this supplies the signature
//! check. Signing and every other algorithm are refused, so an `alg` downgrade (HS256 with the
//! public key as secret, `none`) cannot verify.

use jsonwebtoken::{
    Algorithm, DecodingKey, DecodingKeyKind, EncodingKey,
    crypto::{CryptoProvider, JwtSigner, JwtVerifier, KeyUtils},
    errors::{Error, ErrorKind},
};

struct Rs256(DecodingKey);

impl signature::Verifier<Vec<u8>> for Rs256 {
    fn verify(&self, msg: &[u8], sig: &Vec<u8>) -> Result<(), signature::Error> {
        let DecodingKeyKind::RsaModulusExponent { n, e } = self.0.kind() else {
            return Err(signature::Error::new());
        };
        ring::signature::RsaPublicKeyComponents {
            n: n.as_slice(),
            e: e.as_slice(),
        }
        .verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, msg, sig)
        .map_err(|_| signature::Error::new())
    }
}

impl JwtVerifier for Rs256 {
    fn algorithm(&self) -> Algorithm {
        Algorithm::RS256
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "jsonwebtoken's factory signature"
)]
fn verifier(alg: &Algorithm, key: &DecodingKey) -> Result<Box<dyn JwtVerifier>, Error> {
    if *alg != Algorithm::RS256 || !matches!(key.kind(), DecodingKeyKind::RsaModulusExponent { .. })
    {
        return Err(Error::from(ErrorKind::InvalidAlgorithm));
    }
    Ok(Box::new(Rs256(key.clone())))
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "jsonwebtoken's factory signature"
)]
fn signer(_: &Algorithm, _: &EncodingKey) -> Result<Box<dyn JwtSigner>, Error> {
    Err(Error::from(ErrorKind::InvalidAlgorithm))
}

static PROVIDER: CryptoProvider = CryptoProvider {
    signer_factory: signer,
    verifier_factory: verifier,
    key_utils: KeyUtils::new_unimplemented(),
};

/// Installs the provider for this process (idempotent).
pub(crate) fn install() {
    let _ = PROVIDER.install_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rs256_with_rsa_components_gets_a_verifier() {
        install();
        let rsa = DecodingKey::from_rsa_raw_components(&[1, 0, 1], &[1, 0, 1]);
        assert!(verifier(&Algorithm::RS256, &rsa).is_ok());
        assert!(verifier(&Algorithm::HS256, &rsa).is_err());
        assert!(verifier(&Algorithm::RS256, &DecodingKey::from_secret(b"k")).is_err());
        assert!(signer(&Algorithm::RS256, &EncodingKey::from_secret(b"k")).is_err());
    }
}
