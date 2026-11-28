//! Native Win32 Authenticode verification (`WinVerifyTrust`) and embedded signer-subject extraction.
//!
//! Zero external crypto crates — uses `wintrust.dll` + `crypt32.dll` via `windows-rs`.
//! Enforces fail-closed policy:
//! - Any tampered, untrusted, revoked/distrusted, or non-PE payload is rejected.
//! - If the currently running executable is Authenticode-signed, downloaded installers MUST also
//!   carry a valid Authenticode signature from the exact same signer subject (blocks unsigned
//!   downgrades and foreign publishers).
//! - If the currently running executable is unsigned (dev or pre-SignPath builds), installers may
//!   be either unsigned (guarded by FIPS 180-4 SHA-256 + GitHub HTTPS allowlist) or validly signed.

use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use std::ptr::null_mut;
use windows::Win32::Foundation::{
    CERT_E_UNTRUSTEDROOT, HANDLE, HWND, TRUST_E_BAD_DIGEST, TRUST_E_EXPLICIT_DISTRUST,
    TRUST_E_NOSIGNATURE, TRUST_E_SUBJECT_FORM_UNKNOWN,
};
use windows::Win32::Security::Cryptography::{
    CERT_CONTEXT, CERT_FIND_SUBJECT_CERT, CERT_INFO, CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
    CERT_QUERY_FORMAT_FLAG_BINARY, CERT_QUERY_OBJECT_FILE, CERT_X500_NAME_STR,
    CMSG_SIGNER_INFO, CMSG_SIGNER_INFO_PARAM, CertCloseStore, CertFindCertificateInStore,
    CertFreeCertificateContext, CertNameToStrW, CryptMsgClose, CryptMsgGetParam,
    CryptQueryObject, HCERTSTORE, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
};
use windows::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CHOICE_FILE, WTD_DISABLE_MD2_MD4, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTD_UICONTEXT_EXECUTE, WinVerifyTrust,
};
use windows::core::PCWSTR;

/// Authenticode signature verification outcome for a Windows installer or executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthenticodeStatus {
    /// Valid embedded Authenticode signature trusted by Windows (`WinVerifyTrust == 0`).
    Valid { signer_subject: String },
    /// Valid signable binary with no embedded signature (`TRUST_E_NOSIGNATURE`).
    Unsigned,
    /// File is not a recognized signable PE/CAB/MSI format (`TRUST_E_SUBJECT_FORM_UNKNOWN`).
    UnknownFormat,
    /// Tampered, untrusted, expired, distrusted, missing, or otherwise invalid signature.
    Invalid { code: u32, reason: String },
}

struct WinTrustStateGuard<'a> {
    hwnd: HWND,
    action_id: &'a mut windows::core::GUID,
    data: &'a mut WINTRUST_DATA,
    verified: bool,
}

impl Drop for WinTrustStateGuard<'_> {
    fn drop(&mut self) {
        if self.verified {
            self.data.dwStateAction = WTD_STATEACTION_CLOSE;
            // SAFETY: Closes state opened by `WinVerifyTrust(..., WTD_STATEACTION_VERIFY)`.
            unsafe {
                let _ = WinVerifyTrust(
                    self.hwnd,
                    self.action_id,
                    (self.data as *mut WINTRUST_DATA).cast(),
                );
            }
        }
    }
}

struct CertStoreGuard(HCERTSTORE);

impl Drop for CertStoreGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` is an owned certificate store handle returned by `CryptQueryObject`.
            unsafe {
                let _ = CertCloseStore(Some(self.0), 0);
            }
        }
    }
}

struct CryptMsgGuard(*mut c_void);

impl Drop for CryptMsgGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is an owned cryptographic message handle returned by `CryptQueryObject`.
            unsafe {
                let _ = CryptMsgClose(Some(self.0));
            }
        }
    }
}

struct CertContextGuard(*const CERT_CONTEXT);

impl Drop for CertContextGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was returned by `CertFindCertificateInStore`.
            unsafe {
                let _ = CertFreeCertificateContext(Some(self.0));
            }
        }
    }
}

fn encode_wide_nul(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn extract_embedded_signer_subject(wide_path: &[u16]) -> Option<String> {
    let mut store = HCERTSTORE::default();
    let mut msg: *mut c_void = null_mut();
    // SAFETY: Queries embedded PKCS7 Authenticode signature from validated path.
    unsafe {
        CryptQueryObject(
            CERT_QUERY_OBJECT_FILE,
            wide_path.as_ptr().cast(),
            CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
            CERT_QUERY_FORMAT_FLAG_BINARY,
            0,
            None,
            None,
            None,
            Some(&mut store),
            Some(&mut msg),
            None,
        )
        .ok()?;
    }
    let _store_guard = CertStoreGuard(store);
    let _msg_guard = CryptMsgGuard(msg);
    if store.is_invalid() || msg.is_null() {
        return None;
    }

    let mut cb_signer = 0u32;
    // SAFETY: Queries byte length of `CMSG_SIGNER_INFO`.
    unsafe {
        CryptMsgGetParam(msg, CMSG_SIGNER_INFO_PARAM, 0, None, &mut cb_signer).ok()?;
    }
    if cb_signer < size_of::<CMSG_SIGNER_INFO>() as u32 {
        return None;
    }

    // `u64` buffer guarantees 8-byte alignment required for `CMSG_SIGNER_INFO` pointer fields.
    let mut buf = vec![0u64; (cb_signer as usize).div_ceil(size_of::<u64>())];
    // SAFETY: `buf` is at least `cb_signer` bytes and 8-byte aligned.
    unsafe {
        CryptMsgGetParam(
            msg,
            CMSG_SIGNER_INFO_PARAM,
            0,
            Some(buf.as_mut_ptr().cast()),
            &mut cb_signer,
        )
        .ok()?;
    }
    let signer_info = unsafe { &*buf.as_ptr().cast::<CMSG_SIGNER_INFO>() };

    let cert_lookup = CERT_INFO {
        Issuer: signer_info.Issuer,
        SerialNumber: signer_info.SerialNumber,
        ..Default::default()
    };
    // SAFETY: `store` and `cert_lookup` are valid for the duration of the lookup.
    let cert_ctx = unsafe {
        CertFindCertificateInStore(
            store,
            X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
            0,
            CERT_FIND_SUBJECT_CERT,
            Some((&cert_lookup as *const CERT_INFO).cast()),
            None,
        )
    };
    if cert_ctx.is_null() {
        return None;
    }
    let _ctx_guard = CertContextGuard(cert_ctx);

    // SAFETY: `cert_ctx` and its `pCertInfo` are valid while `_ctx_guard` is alive.
    let cert_info_ref = unsafe {
        let info = (*cert_ctx).pCertInfo;
        if info.is_null() {
            return None;
        }
        &(*info).Subject
    };
    // SAFETY: First call with `None` returns required UTF-16 buffer length including NUL.
    let needed = unsafe { CertNameToStrW(X509_ASN_ENCODING, cert_info_ref, CERT_X500_NAME_STR, None) };
    if needed <= 1 {
        return None;
    }
    let mut subject_buf = vec![0u16; needed as usize];
    // SAFETY: `subject_buf` has capacity `needed` UTF-16 code units.
    let written = unsafe {
        CertNameToStrW(
            X509_ASN_ENCODING,
            cert_info_ref,
            CERT_X500_NAME_STR,
            Some(&mut subject_buf),
        )
    };
    if written <= 1 {
        return None;
    }
    let end = subject_buf.iter().position(|&c| c == 0).unwrap_or(subject_buf.len());
    let subject = String::from_utf16_lossy(&subject_buf[..end]).trim().to_owned();
    if subject.is_empty() {
        None
    } else {
        Some(subject)
    }
}

/// Verifies the Authenticode signature of `path` using Win32 `WinVerifyTrust` (`WTD_UI_NONE`).
pub fn verify_authenticode_signature(path: &Path) -> AuthenticodeStatus {
    if !path.is_file() {
        return AuthenticodeStatus::Invalid {
            code: 0x8007_0002,
            reason: "file_not_found".to_owned(),
        };
    }

    let wide = encode_wide_nul(path);
    let mut action_id = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide.as_ptr()),
        hFile: HANDLE::default(),
        pgKnownSubject: null_mut(),
    };
    let mut wvt_data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_DISABLE_MD2_MD4,
        dwUIContext: WTD_UICONTEXT_EXECUTE,
        ..Default::default()
    };

    let invalid_hwnd = HWND(-1isize as *mut c_void);
    let status = {
        let mut guard = WinTrustStateGuard {
            hwnd: invalid_hwnd,
            action_id: &mut action_id,
            data: &mut wvt_data,
            verified: false,
        };
        // SAFETY: `file_info` and `wide` stay alive across `WinVerifyTrust` and `guard` drop.
        let rc = unsafe {
            WinVerifyTrust(
                guard.hwnd,
                guard.action_id,
                (guard.data as *mut WINTRUST_DATA).cast(),
            )
        };
        guard.verified = true;
        rc
    };

    if status == 0 {
        return match extract_embedded_signer_subject(&wide) {
            Some(signer_subject) => AuthenticodeStatus::Valid { signer_subject },
            None => AuthenticodeStatus::Invalid {
                code: 0,
                reason: "missing_signer_subject".to_owned(),
            },
        };
    }

    if status == TRUST_E_NOSIGNATURE.0 {
        return AuthenticodeStatus::Unsigned;
    }
    if status == TRUST_E_SUBJECT_FORM_UNKNOWN.0 {
        return AuthenticodeStatus::UnknownFormat;
    }

    let code = status as u32;
    let reason = if status == TRUST_E_BAD_DIGEST.0 {
        format!("bad_digest({code:#010X})")
    } else if status == CERT_E_UNTRUSTEDROOT.0 {
        format!("untrusted_root({code:#010X})")
    } else if status == TRUST_E_EXPLICIT_DISTRUST.0 {
        format!("explicit_distrust({code:#010X})")
    } else {
        format!("wintrust_{code:#010X}")
    };
    AuthenticodeStatus::Invalid { code, reason }
}

/// Evaluates fail-closed Authenticode policy between the running app (`self_sig`) and a
/// downloaded installer (`installer_sig`).
pub fn evaluate_installer_signature_policy(
    self_sig: &AuthenticodeStatus,
    installer_sig: &AuthenticodeStatus,
) -> Result<(), String> {
    if let AuthenticodeStatus::Invalid { reason, .. } = installer_sig {
        return Err(format!("installer signature is invalid: {reason}"));
    }
    if let AuthenticodeStatus::Invalid { reason, .. } = self_sig {
        return Err(format!("running executable signature is invalid: {reason}"));
    }
    if matches!(installer_sig, AuthenticodeStatus::UnknownFormat) {
        return Err("downloaded installer is not a valid Windows executable format".to_owned());
    }

    match self_sig {
        AuthenticodeStatus::Valid {
            signer_subject: self_sub,
        } => {
            let self_trimmed = self_sub.trim();
            if self_trimmed.is_empty() {
                return Err("running executable has an empty signer subject".to_owned());
            }
            match installer_sig {
                AuthenticodeStatus::Valid {
                    signer_subject: inst_sub,
                } => {
                    let inst_trimmed = inst_sub.trim();
                    if inst_trimmed.is_empty() {
                        return Err("installer signature has an empty signer subject".to_owned());
                    }
                    if !self_trimmed.eq_ignore_ascii_case(inst_trimmed) {
                        return Err(format!(
                            "installer publisher mismatch: expected '{self_trimmed}', got '{inst_trimmed}'"
                        ));
                    }
                    Ok(())
                }
                AuthenticodeStatus::Unsigned => Err(
                    "refusing unsigned installer upgrade while running a signed build".to_owned(),
                ),
                AuthenticodeStatus::UnknownFormat | AuthenticodeStatus::Invalid { .. } => {
                    unreachable!("handled above")
                }
            }
        }
        AuthenticodeStatus::Unsigned | AuthenticodeStatus::UnknownFormat => match installer_sig {
            AuthenticodeStatus::Unsigned => Ok(()),
            AuthenticodeStatus::Valid { signer_subject } => {
                if signer_subject.trim().is_empty() {
                    Err("installer signature has an empty signer subject".to_owned())
                } else {
                    Ok(())
                }
            }
            AuthenticodeStatus::UnknownFormat | AuthenticodeStatus::Invalid { .. } => {
                unreachable!("handled above")
            }
        },
        AuthenticodeStatus::Invalid { .. } => unreachable!("handled above"),
    }
}

/// Verifies `installer_path` with `WinVerifyTrust` and checks it against the currently running
/// executable's Authenticode status.
pub fn verify_installer_against_current_exe(
    installer_path: &Path,
) -> Result<AuthenticodeStatus, String> {
    let current_exe = std::env::current_exe()
        .map_err(|e| format!("failed to resolve current exe for Authenticode check: {e}"))?;
    let self_sig = verify_authenticode_signature(&current_exe);
    let installer_sig = verify_authenticode_signature(installer_path);
    evaluate_installer_signature_policy(&self_sig, &installer_sig)?;
    Ok(installer_sig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_path(name: &str) -> std::path::PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gh_win32_auth_{}_{}_{name}",
            std::process::id(),
            seq
        ))
    }

    #[test]
    fn policy_enforces_all_signed_unsigned_downgrade_foreign_and_invalid_cases() {
        let signed_a = AuthenticodeStatus::Valid {
            signer_subject: "CN=SignPath OSS, O=Genshin-Helper".to_owned(),
        };
        let signed_a_case = AuthenticodeStatus::Valid {
            signer_subject: "cn=signpath oss, o=genshin-helper".to_owned(),
        };
        let signed_b = AuthenticodeStatus::Valid {
            signer_subject: "CN=Foreign Publisher".to_owned(),
        };
        let signed_empty = AuthenticodeStatus::Valid {
            signer_subject: "   ".to_owned(),
        };
        let unsigned = AuthenticodeStatus::Unsigned;
        let unknown = AuthenticodeStatus::UnknownFormat;
        let invalid = AuthenticodeStatus::Invalid {
            code: 0x8009_6010,
            reason: "bad_digest".to_owned(),
        };

        assert!(evaluate_installer_signature_policy(&signed_a, &signed_a).is_ok());
        assert!(evaluate_installer_signature_policy(&signed_a, &signed_a_case).is_ok());
        assert!(evaluate_installer_signature_policy(&signed_a, &signed_b).is_err());
        assert!(evaluate_installer_signature_policy(&signed_a, &unsigned).is_err());
        assert!(evaluate_installer_signature_policy(&signed_a, &signed_empty).is_err());
        assert!(evaluate_installer_signature_policy(&unsigned, &unsigned).is_ok());
        assert!(evaluate_installer_signature_policy(&unsigned, &signed_a).is_ok());
        assert!(evaluate_installer_signature_policy(&unsigned, &unknown).is_err());
        assert!(evaluate_installer_signature_policy(&unsigned, &invalid).is_err());
        assert!(evaluate_installer_signature_policy(&invalid, &unsigned).is_err());
    }

    #[test]
    fn live_verify_authenticode_handles_unsigned_current_exe_non_pe_and_missing_file() {
        let current = std::env::current_exe().expect("current test exe");
        assert_eq!(verify_authenticode_signature(&current), AuthenticodeStatus::Unsigned);

        let non_pe = temp_path("not_pe.txt");
        std::fs::write(&non_pe, b"hello plain text not an executable").expect("write non-pe");
        assert_eq!(
            verify_authenticode_signature(&non_pe),
            AuthenticodeStatus::UnknownFormat
        );
        let _ = std::fs::remove_file(&non_pe);

        let missing = temp_path("does_not_exist.exe");
        assert!(matches!(
            verify_authenticode_signature(&missing),
            AuthenticodeStatus::Invalid { .. }
        ));
    }

    #[test]
    fn live_verify_authenticode_validates_embedded_signed_binary_and_rejects_tamper() {
        let candidates = [
            Path::new(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            Path::new(r"C:\Windows\explorer.exe"),
        ];
        let mut verified_src = None;
        for cand in candidates {
            if cand.is_file()
                && let AuthenticodeStatus::Valid { signer_subject } =
                    verify_authenticode_signature(cand)
            {
                assert!(!signer_subject.is_empty());
                verified_src = Some((cand.to_path_buf(), signer_subject));
                break;
            }
        }
        let Some((src_exe, _subject)) = verified_src else {
            return;
        };

        let copy_path = temp_path("signed_copy.exe");
        let mut bytes = std::fs::read(&src_exe).expect("read signed exe");
        std::fs::write(&copy_path, &bytes).expect("write clean copy");
        assert!(matches!(
            verify_authenticode_signature(&copy_path),
            AuthenticodeStatus::Valid { .. }
        ));

        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&copy_path, &bytes).expect("write tampered copy");
        assert!(
            matches!(
                verify_authenticode_signature(&copy_path),
                AuthenticodeStatus::Invalid { .. }
            ),
            "tampered copy of signed executable must fail WinVerifyTrust"
        );
        let _ = std::fs::remove_file(&copy_path);
    }
}
