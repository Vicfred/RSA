use crate::algorithms::copy_with_left_pad;
use crate::internals;
use crate::key::{RSAPrivateKey, RSAPublicKey};
use crate::errors::{Error, Result};

use std::vec::Vec;
use num_bigint::BigUint;
use subtle::ConstantTimeEq;
use digest::Digest;
use rand::Rng;

pub fn verify<H: Digest>(
    pub_key: &RSAPublicKey,
    hashed: &[u8],
    sig: &[u8]) -> Result<()>
{
    let n_bits = pub_key.n().bits();
    if sig.len() != (n_bits + 7) / 8 {
        return Err(Error::Verification);
    }
    let s = BigUint::from_bytes_be(sig);
    let m = internals::encrypt(pub_key, &s).to_bytes_be();
    let em_bits = n_bits - 1;
    let em_len = (em_bits + 7) / 8;

    if em_len < m.len() {
        return Err(Error::Verification);
    }

    let mut em = vec![0; em_len];
    copy_with_left_pad(&mut em, &m);

    emsa_pss_verify::<H>(hashed, &mut em, em_bits, None)
}


/// SignPSS calculates the signature of hashed using RSASSA-PSS [1].
/// Note that hashed must be the result of hashing the input message using the
/// given hash function. The opts argument may be nil, in which case sensible
/// defaults are used.
pub fn sign<T: Rng, H: Digest>(rng: &mut T, priv_key: &RSAPrivateKey, hashed: &[u8], salt_len: Option<usize>, blind: bool) -> Result<Vec<u8>> {
    let salt_len = salt_len.unwrap_or_else(|| {
        (priv_key.n().bits() + 7) / 8 - 2 - H::output_size()
    });

    let mut salt = vec![0; salt_len];
    rng.fill(&mut salt[..]);

    return sign_pss_with_salt::<_, H>(rng, priv_key, hashed, &salt, blind)
}


// signPSSWithSalt calculates the signature of hashed using PSS [1] with specified salt.
// Note that hashed must be the result of hashing the input message using the
// given hash function. salt is a random sequence of bytes whose length will be
// later used to verify the signature.
fn sign_pss_with_salt<T: Rng, H: Digest>(rng: &mut T, priv_key: &RSAPrivateKey, hashed: &[u8], salt: &[u8], blind: bool) -> Result<Vec<u8>> {
    let n_bits = priv_key.n().bits();
    let mut em = vec![0; ((n_bits - 1) + 7) / 8];
    emsa_pss_encode::<H>(&mut em, hashed, n_bits - 1, salt)?;

    let m = BigUint::from_bytes_be(&em);

    let blind_rng = if blind {
        Some(rng)
    } else {
        None
    };

    let c = internals::decrypt_and_check(blind_rng, priv_key, &m)?.to_bytes_be();

    let mut s = vec![0; (n_bits + 7) / 8];
    copy_with_left_pad(&mut s, &c);
    return Ok(s)
}

fn emsa_pss_encode<H: Digest>(em: &mut [u8], m_hash: &[u8], em_bits: usize, salt: &[u8]) -> Result<()> {
    // See [1], section 9.1.1
    let h_len = H::output_size();
    let s_len = salt.len();
    let em_len = (em_bits + 7) / 8;

    // 1. If the length of M is greater than the input limitation for the
    //     hash function (2^61 - 1 octets for SHA-1), output "message too
    //     long" and stop.
    //
    // 2.  Let mHash = Hash(M), an octet string of length hLen.
    if m_hash.len() != h_len {
        return Err(Error::InputNotHashed);
    }

    // 3. If em_len < h_len + s_len + 2, output "encoding error" and stop.
    if em_len < h_len + s_len + 2 {
        // TODO: Key size too small
        return Err(Error::Internal);
    }

    if em.len() != em_len {
        return Err(Error::Internal);
    }

    let (db, h) = em.split_at_mut(em_len - s_len - h_len - 2 + 1 + s_len);
    let h = &mut h[..(em_len - 1) - db.len()];

    // 4. Generate a random octet string salt of length s_len; if s_len = 0,
    //     then salt is the empty string.
    //
    // 5.  Let
    //       M' = (0x)00 00 00 00 00 00 00 00 || m_hash || salt;
    //
    //     M' is an octet string of length 8 + h_len + s_len with eight
    //     initial zero octets.
    //
    // 6.  Let H = Hash(M'), an octet string of length h_len.
    let prefix = [0u8; 8];
    let mut hash = H::new();

    hash.input(&prefix);
    hash.input(m_hash);
    hash.input(salt);

    let hashed = hash.result();
    h.copy_from_slice(&hashed);

    // 7.  Generate an octet string PS consisting of em_len - s_len - h_len - 2
    //     zero octets. The length of PS may be 0.
    //
    // 8.  Let DB = PS || 0x01 || salt; DB is an octet string of length
    //     emLen - hLen - 1.
    db[em_len - s_len - h_len - 2] = 0x01;
    db[em_len - s_len - h_len - 1..].copy_from_slice(salt);

    // 9.  Let dbMask = MGF(H, emLen - hLen - 1).
    //
    // 10. Let maskedDB = DB \xor dbMask.
    mgf1_xor(db, &mut H::new(), &h);

    // 11. Set the leftmost 8 * em_len - em_bits bits of the leftmost octet in
    //     maskedDB to zero.
    db[0] &= 0xFF >> (8 * em_len - em_bits);

    // 12. Let EM = maskedDB || H || 0xbc.
    em[em_len-1] = 0xBC;

    return Ok(())
}

fn emsa_pss_verify<H: Digest>(m_hash: &[u8], em: &mut [u8], em_bits: usize, s_len: Option<usize>) -> Result<()> {
    // 1. If the length of M is greater than the input limitation for the
    //    hash function (2^61 - 1 octets for SHA-1), output "inconsistent"
    //    and stop.
    //
    // 2. Let mHash = Hash(M), an octet string of length hLen
    let h_len = H::output_size();
    if m_hash.len() != h_len {
        return Err(Error::Verification);
    }

    // 3. If emLen < hLen + sLen + 2, output "inconsistent" and stop.
    let em_len = em.len();//(em_bits + 7) / 8;
    if em_len < h_len + 2 {
        return Err(Error::Verification)
    }

    // 4. If the rightmost octet of EM does not have hexadecimal value
    //    0xbc, output "inconsistent" and stop.
    if em[em.len() - 1] != 0xBC {
        return Err(Error::Verification)
    }

    // 5. Let maskedDB be the leftmost emLen - hLen - 1 octets of EM, and
    //    let H be the next hLen octets.
    let (db, h) = em.split_at_mut(em_len - h_len - 1);
    let h = &mut h[..(em_len - 1) - (em_len - h_len - 1)];

    // 6. If the leftmost 8 * em_len - em_bits bits of the leftmost octet in
    //    maskedDB are not all equal to zero, output "inconsistent" and
    //    stop.
    if db[0] & (0xFF << /*uint*/(8 - (8 * em_len - em_bits))) != 0 {
        return Err(Error::Verification)
    }

    // 7. Let dbMask = MGF(H, em_len - h_len - 1)
    //
    // 8. Let DB = maskedDB \xor dbMask
    mgf1_xor(db, &mut H::new(), &*h);


    // 9.  Set the leftmost 8 * emLen - emBits bits of the leftmost octet in DB
    //     to zero.
    db[0] &= 0xFF >> /*uint*/(8 * em_len - em_bits);

    let s_len = match s_len {
        None => (0..=em_len - (h_len + 2)).rev().try_fold(None, |state, i| {
            match (state, db[em_len - h_len - i - 2]) {
                (Some(i), _) => Ok(Some(i)),
                (_, 1) => Ok(Some(i)),
                (_, 0) => Ok(None),
                _ => Err(Error::Verification)
            }
        })?.ok_or(Error::Verification)?,
        Some(s_len) => {
            // 10. If the emLen - hLen - sLen - 2 leftmost octets of DB are not zero
            //     or if the octet at position emLen - hLen - sLen - 1 (the leftmost
            //     position is "position 1") does not have hexadecimal value 0x01,
            //     output "inconsistent" and stop.
            for e in &db[..em_len - h_len - s_len - 2] {
                if *e != 0x00 {
                    return Err(Error::Verification);
                }
            }
            if db[em_len - h_len - s_len - 2] != 0x01 {
                return Err(Error::Verification)
            }
            s_len
        }
    };

    // 11. Let salt be the last s_len octets of DB.
    let salt = &db[db.len() - s_len..];

    // 12. Let
    //          M' = (0x)00 00 00 00 00 00 00 00 || mHash || salt ;
    //     M' is an octet string of length 8 + hLen + sLen with eight
    //     initial zero octets.
    //
    // 13. Let H' = Hash(M'), an octet string of length hLen.
    let prefix = [0u8; 8];

    let mut hash = H::new();
    hash.input(prefix);
    hash.input(m_hash);
    hash.input(salt);
    let h0 = hash.result();

    // 14. If H = H', output "consistent." Otherwise, output "inconsistent."
    if Into::<bool>::into(h0.ct_eq(h)) {
        Ok(())
    } else {
        Err(Error::Verification)
    }
}

fn inc_counter(counter: &mut [u8]) {
    if counter[3] == u8::max_value() {
        counter[3] = 0;
    } else {
        counter[3] += 1;
        return;
    }

    if counter[2] == u8::max_value() {
        counter[2] = 0;
    } else {
        counter[2] += 1;
        return;
    }

    if counter[1] == u8::max_value() {
        counter[1] = 0;
    } else {
        counter[1] += 1;
        return;
    }

    if counter[0] == u8::max_value() {
        counter[0] = 0u8;
        counter[1] = 0u8;
        counter[2] = 0u8;
        counter[3] = 0u8;
    } else {
        counter[0] += 1;
    }
}

/// Mask generation function
///
/// Will reset the Digest before returning.
fn mgf1_xor<T: Digest>(out: &mut [u8], digest: &mut T, seed: &[u8]) {
    let mut counter = vec![0u8; 4];
    let mut i = 0;

    while i < out.len() {
        let mut digest_input = vec![0u8; seed.len() + 4];
        digest_input[0..seed.len()].copy_from_slice(seed);
        digest_input[seed.len()..].copy_from_slice(&counter);

        digest.input(digest_input.as_slice());
        let digest_output = &*digest.result_reset();
        let mut j = 0;
        loop {
            if j >= digest_output.len() || i >= out.len() {
                break;
            }

            out[i] ^= digest_output[j];
            j += 1;
            i += 1;
        }
        inc_counter(counter.as_mut_slice());
    }
}

#[cfg(test)]
mod test {
    use crate::{RSAPrivateKey, RSAPublicKey};

    use num_bigint::BigUint;
    use num_traits::{FromPrimitive, Num};
    use sha1::{Digest, Sha1};
    use rand::thread_rng;

    fn get_private_key() -> RSAPrivateKey {
        // In order to generate new test vectors you'll need the PEM form of this key:
        // -----BEGIN RSA PRIVATE KEY-----
        // MIIBOgIBAAJBALKZD0nEffqM1ACuak0bijtqE2QrI/KLADv7l3kK3ppMyCuLKoF0
        // fd7Ai2KW5ToIwzFofvJcS/STa6HA5gQenRUCAwEAAQJBAIq9amn00aS0h/CrjXqu
        // /ThglAXJmZhOMPVn4eiu7/ROixi9sex436MaVeMqSNf7Ex9a8fRNfWss7Sqd9eWu
        // RTUCIQDasvGASLqmjeffBNLTXV2A5g4t+kLVCpsEIZAycV5GswIhANEPLmax0ME/
        // EO+ZJ79TJKN5yiGBRsv5yvx5UiHxajEXAiAhAol5N4EUyq6I9w1rYdhPMGpLfk7A
        // IU2snfRJ6Nq2CQIgFrPsWRCkV+gOYcajD17rEqmuLrdIRexpg8N1DOSXoJ8CIGlS
        // tAboUGBxTDq3ZroNism3DaMIbKPyYrAqhKov1h5V
        // -----END RSA PRIVATE KEY-----

        RSAPrivateKey::from_components(
            BigUint::from_str_radix("9353930466774385905609975137998169297361893554149986716853295022578535724979677252958524466350471210367835187480748268864277464700638583474144061408845077", 10).unwrap(),
            BigUint::from_u64(65537).unwrap(),
            BigUint::from_str_radix("7266398431328116344057699379749222532279343923819063639497049039389899328538543087657733766554155839834519529439851673014800261285757759040931985506583861", 10).unwrap(),
            vec![
                BigUint::from_str_radix("98920366548084643601728869055592650835572950932266967461790948584315647051443",10).unwrap(),
                BigUint::from_str_radix("94560208308847015747498523884063394671606671904944666360068158221458669711639", 10).unwrap()
            ],
        )
    }

    #[test]
    fn test_verify_pss() {
        let priv_key = get_private_key();

        let tests = [[
            "test\n", "6f86f26b14372b2279f79fb6807c49889835c204f71e38249b4c5601462da8ae30f26ffdd9c13f1c75eee172bebe7b7c89f2f1526c722833b9737d6c172a962f"
        ]];
        let pub_key: RSAPublicKey = priv_key.into();

        for test in &tests {
            let digest = Sha1::digest(test[0].as_bytes()).to_vec();
            let sig = hex::decode(test[1]).unwrap();

            pub_key
                .verify_pss::<Sha1>(&digest, &sig)
                .expect("failed to verify");
        }
    }

    #[test]
    fn test_sign_and_verify_roundtrip() {
        let priv_key = get_private_key();

        let tests = ["test\n"];

        for test in &tests {
            let digest = Sha1::digest(test.as_bytes()).to_vec();
            let sig = priv_key
                .sign_pss::<Sha1, _>(&mut thread_rng(), &digest, None, true)
                .expect("failed to sign");

            priv_key
                .verify_pss::<Sha1>(&digest, &sig)
                .expect("failed to verify");
        }
    }
}