//! VS Code 系桌面客户端（Windows Electron Safe Storage）加解密工具集。
//!
//! 提供 Windows 平台下 `Local State` → DPAPI → AES-256-GCM `v10` 的机密读写能力，
//! 供 WorkBuddy 与同类 VS Code fork 客户端（如 Qoder）复用。加密模型对齐
//! Chromium/Electron Safe Storage：
//! - Windows: Local State `os_crypt.encrypted_key` + DPAPI → AES-256-GCM `v10`

#[cfg(target_os = "windows")]
use aes_gcm::aead::generic_array::GenericArray;
#[cfg(target_os = "windows")]
use aes_gcm::aead::{Aead, AeadCore, OsRng};
#[cfg(target_os = "windows")]
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
#[cfg(target_os = "windows")]
use base64::{engine::general_purpose, Engine as _};

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{LocalFree, HLOCAL};
#[cfg(target_os = "windows")]
use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

const V10_PREFIX: &[u8] = b"v10";

#[cfg(target_os = "windows")]
pub(crate) fn dpapi_decrypt(encrypted: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let mut data_in = CRYPT_INTEGER_BLOB {
            cbData: encrypted.len() as u32,
            pbData: encrypted.as_ptr() as *mut u8,
        };
        let mut data_out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        CryptUnprotectData(
            &mut data_in,
            None,
            None,
            None,
            None,
            0,
            &mut data_out,
        )
        .map_err(|e| format!("DPAPI CryptUnprotectData failed: {e}"))?;
        if data_out.pbData.is_null() || data_out.cbData == 0 {
            return Err("DPAPI returned empty data".to_string());
        }
        let slice = std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize);
        let result = slice.to_vec();
        let _ = LocalFree(HLOCAL(data_out.pbData as _));
        Ok(result)
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn decrypt_windows_gcm_v10(key: &[u8], encrypted: &[u8]) -> Result<Vec<u8>, String> {
    if encrypted.len() < 31 {
        return Err("ciphertext too short for AES-GCM".to_string());
    }
    if &encrypted[..3] != V10_PREFIX {
        return Err(format!(
            "Unexpected ciphertext prefix: {:?}",
            &encrypted[..3]
        ));
    }
    let nonce_bytes = &encrypted[3..15];
    let ciphertext = &encrypted[15..];
    let cipher = Aes256Gcm::new(GenericArray::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| format!("AES-GCM decryption failed: {e}"))
}

#[cfg(target_os = "windows")]
pub(crate) fn encrypt_windows_gcm_v10(key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new(GenericArray::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| format!("AES-GCM encryption failed: {e}"))?;
    let mut result = Vec::with_capacity(3 + nonce.len() + ciphertext.len());
    result.extend_from_slice(V10_PREFIX);
    result.extend_from_slice(&nonce);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}
