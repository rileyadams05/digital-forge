//! Validate the key-recovery path supported by ShadPKG before starting its unsafe
//! native parser. This is a capability check, not retail-PKG decryption support.
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::NoPadding};
use anyhow::{Context, Result, ensure};
use rsa::Pkcs1v15Encrypt;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

fn digest_matches(key: &[u8], expected: &[u8]) -> bool {
    key.len() == 32
        && expected.len() == 32
        && Sha256::digest(key)
            .iter()
            .zip(key)
            .zip(expected)
            .all(|((hash, key), expected)| hash ^ key == *expected)
}

fn validate_records(entry: &[u8; 32], records: &[u8], image: &[u8]) -> Result<()> {
    ensure!(records.len() >= 2048, "truncated PS4 ENTRY_KEYS record");
    ensure!(image.len() == 256, "invalid PS4 IMAGE_KEY size");
    let dk3 = orbis_pkg::keys::pkg_key3()
        .decrypt(Pkcs1v15Encrypt, &records[1024..1280])
        .context("PS4 metadata-key RSA decoding failed; extraction was not started")?;
    ensure!(
        digest_matches(&dk3, &records[128..160]),
        "PS4 metadata-key digest mismatch"
    );
    let mut input = Sha256::new();
    input.update(entry);
    input.update(&dk3);
    let iv_key = input.finalize();
    let mut image = image.to_vec();
    let plaintext = cbc::Decryptor::<aes::Aes128>::new_from_slices(&iv_key[16..], &iv_key[..16])
        .expect("fixed AES key and IV lengths")
        .decrypt_padded_mut::<NoPadding>(&mut image)
        .map_err(|_| anyhow::anyhow!("invalid encrypted PS4 IMAGE_KEY block"))?;
    let ekpfs = orbis_pkg::keys::fake_pfs_key().decrypt(Pkcs1v15Encrypt, plaintext)
        .map_err(|_| anyhow::anyhow!(
            "PS4 filesystem key is unavailable to ShadPKG: IMAGE_KEY RSA decoding failed with its supported fake-package key. Package metadata is readable, but the encrypted game/update payload cannot be decrypted by this backend. Extraction was not started, preventing ShadPKG's invalid-key crash. The original package is preserved; no update has been prepared or applied."
        ))?;
    ensure!(
        digest_matches(&ekpfs, &records[64..96]),
        "PS4 filesystem-key digest mismatch; extraction was not started"
    );
    Ok(())
}

pub(super) fn validate_package(source: &Path, no_data_addon: bool) -> Result<()> {
    let mut file = File::open(source)?;
    let length = file.metadata()?.len();
    validate_reader(&mut file, length, no_data_addon)
}

fn validate_reader(file: &mut (impl Read + Seek), length: u64, no_data_addon: bool) -> Result<()> {
    let mut header = [0u8; 0x440];
    file.read_exact(&mut header)
        .context("truncated PS4 package header")?;
    ensure!(&header[..4] == b"\x7fCNT", "invalid PS4 package magic");
    let u32_at = |offset| u32::from_be_bytes(header[offset..offset + 4].try_into().unwrap()) as u64;
    let count = u32_at(0x10);
    let table = u32_at(0x18);
    ensure!(
        count > 0
            && count <= 4096
            && table >= header.len() as u64
            && table
                .checked_add(count * 32)
                .is_some_and(|end| end <= length),
        "invalid PS4 package entry table bounds"
    );
    let pfs_size = u64::from_be_bytes(header[0x418..0x420].try_into().unwrap());
    if no_data_addon && pfs_size == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(table))?;
    let mut key_entry = None;
    let mut image_entry = None;
    for _ in 0..count {
        let mut entry = [0u8; 32];
        file.read_exact(&mut entry)?;
        let id = u32::from_be_bytes(entry[..4].try_into().unwrap());
        let slot = match id {
            0x10 => &mut key_entry,
            0x20 => &mut image_entry,
            _ => continue,
        };
        ensure!(slot.is_none(), "duplicate PS4 key entry {id:#x}");
        *slot = Some(entry);
    }
    let mut read_entry = |entry: &[u8; 32], minimum, maximum| -> Result<Vec<u8>> {
        let offset = u32::from_be_bytes(entry[16..20].try_into().unwrap()) as u64;
        let size = u32::from_be_bytes(entry[20..24].try_into().unwrap()) as u64;
        ensure!(
            size >= minimum
                && size <= maximum
                && offset >= header.len() as u64
                && offset.checked_add(size).is_some_and(
                    |end| end <= length && (end <= table || offset >= table + count * 32)
                ),
            "invalid PS4 key entry bounds or size"
        );
        file.seek(SeekFrom::Start(offset))?;
        let mut data = vec![0; size as usize];
        file.read_exact(&mut data)?;
        Ok(data)
    };
    let keys = read_entry(&key_entry.context("PS4 ENTRY_KEYS is missing")?, 2048, 4096)?;
    let image_entry = image_entry.context("PS4 IMAGE_KEY is missing")?;
    let image = read_entry(&image_entry, 256, 256)?;
    validate_records(&image_entry, &keys, &image)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;
    use rsa::{RsaPublicKey, rand_core::OsRng};
    use std::io::Cursor;

    fn fixture(supported: bool) -> Vec<u8> {
        let mut data = vec![0u8; 0x1600];
        data[..4].copy_from_slice(b"\x7fCNT");
        data[0x10..0x14].copy_from_slice(&2u32.to_be_bytes());
        data[0x18..0x1c].copy_from_slice(&0x500u32.to_be_bytes());
        data[0x418..0x420].copy_from_slice(&0x1000u64.to_be_bytes());
        for (slot, id, offset, size) in [
            (0x500, 0x10u32, 0x600u32, 2048u32),
            (0x520, 0x20, 0xe00, 256),
        ] {
            data[slot..slot + 4].copy_from_slice(&id.to_be_bytes());
            data[slot + 16..slot + 20].copy_from_slice(&offset.to_be_bytes());
            data[slot + 20..slot + 24].copy_from_slice(&size.to_be_bytes());
        }
        let dk3 = [0x31u8; 32];
        let ekpfs = [0x52u8; 32];
        for (index, key) in [(3, dk3), (1, ekpfs)] {
            let digest = Sha256::digest(key);
            for i in 0..32 {
                data[0x600 + 32 + index * 32 + i] = digest[i] ^ key[i];
            }
        }
        let ciphertext = RsaPublicKey::from(orbis_pkg::keys::pkg_key3())
            .encrypt(&mut OsRng, Pkcs1v15Encrypt, &dk3)
            .unwrap();
        data[0xa00..0xb00].copy_from_slice(&ciphertext);
        let mut image = if supported {
            RsaPublicKey::from(orbis_pkg::keys::fake_pfs_key())
                .encrypt(&mut OsRng, Pkcs1v15Encrypt, &ekpfs)
                .unwrap()
        } else {
            vec![0; 256]
        };
        let mut sha = Sha256::new();
        sha.update(&data[0x520..0x540]);
        sha.update(dk3);
        let iv_key = sha.finalize();
        cbc::Encryptor::<aes::Aes128>::new_from_slices(&iv_key[16..], &iv_key[..16])
            .unwrap()
            .encrypt_padded_mut::<NoPadding>(&mut image, 256)
            .unwrap();
        data[0xe00..0xf00].copy_from_slice(&image);
        data
    }
    #[test]
    fn valid_fake_package_keys_pass() {
        let data = fixture(true);
        let len = data.len() as u64;
        validate_reader(&mut Cursor::new(data), len, false).unwrap();
    }
    #[test]
    fn valid_dlc_keys_can_precede_the_file_table() {
        let mut data = fixture(true);
        data.copy_within(0x500..0x540, 0x1500);
        data[0x18..0x1c].copy_from_slice(&0x1500u32.to_be_bytes());
        let len = data.len() as u64;
        validate_reader(&mut Cursor::new(data), len, false).unwrap();
    }
    #[test]
    fn unsupported_image_key_is_rejected_before_native_extraction() {
        let data = fixture(false);
        let len = data.len() as u64;
        let error = validate_reader(&mut Cursor::new(data), len, false).unwrap_err();
        assert!(error.to_string().contains("IMAGE_KEY RSA decoding failed"));
    }
    #[test]
    fn corrupt_key_digest_is_rejected() {
        let mut data = fixture(true);
        data[0x680] ^= 1;
        let len = data.len() as u64;
        assert!(
            validate_reader(&mut Cursor::new(data), len, false)
                .unwrap_err()
                .to_string()
                .contains("digest mismatch")
        );
    }
    #[test]
    fn malformed_key_entry_cannot_allocate_unbounded_memory() {
        let mut data = fixture(true);
        data[0x514..0x518].copy_from_slice(&u32::MAX.to_be_bytes());
        let len = data.len() as u64;
        assert!(
            validate_reader(&mut Cursor::new(data), len, false)
                .unwrap_err()
                .to_string()
                .contains("bounds")
        );
    }
}
