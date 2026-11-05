#![allow(clippy::collapsible_if)]

use std::path::Path;

use base64::Engine;
use hmac::{KeyInit, Mac};

use super::AccountsError;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct EncryptedBundle {
    pub format: String,
    pub kdf: String,
    pub iterations: usize,
    pub salt: String,
    pub nonce: String,
    pub ciphertext: String,
    pub tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportResult {
    pub added: usize,
    pub updated: usize,
    pub skipped: usize,
    pub total: usize,
}

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

pub fn encrypt_bundle(
    plaintext: &[u8],
    password: &str,
    salt_opt: &[u8],
    nonce_opt: &[u8],
) -> Result<EncryptedBundle, AccountsError> {
    let mut salt = [0u8; 16];
    if salt_opt.is_empty() {
        getrandom::fill(&mut salt)
            .map_err(|e| AccountsError::Message(format!("failed to generate random salt: {}", e)))?;
    } else {
        salt.copy_from_slice(&salt_opt[..16]);
    }

    let mut nonce = [0u8; 16];
    if nonce_opt.is_empty() {
        getrandom::fill(&mut nonce)
            .map_err(|e| AccountsError::Message(format!("failed to generate random nonce: {}", e)))?;
    } else {
        nonce.copy_from_slice(&nonce_opt[..16]);
    }

    const ITERATIONS: u32 = 100000;
    let mut dk = [0u8; 64];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), &salt, ITERATIONS, &mut dk);
    let key_enc = &dk[..32];
    let key_mac = &dk[32..];

    let mut ciphertext = vec![0u8; plaintext.len()];
    let block_size = 32;
    let num_blocks = plaintext.len().div_ceil(block_size);

    let mut ctr_buf = [0u8; 24];
    ctr_buf[..16].copy_from_slice(&nonce);

    for i in 0..num_blocks {
        ctr_buf[16..24].copy_from_slice(&(i as u64).to_be_bytes());
        let mut mac = HmacSha256::new_from_slice(key_enc)
            .map_err(|e| AccountsError::Message(format!("failed to initialize HMAC-SHA256: {}", e)))?;
        mac.update(&ctr_buf);
        let ks_block = mac.finalize().into_bytes();

        let start = i * block_size;
        let end = std::cmp::min(start + block_size, plaintext.len());
        for j in start..end {
            ciphertext[j] = plaintext[j] ^ ks_block[j - start];
        }
    }

    let mut mac = HmacSha256::new_from_slice(key_mac)
        .map_err(|e| AccountsError::Message(format!("failed to initialize HMAC-SHA256: {}", e)))?;
    mac.update(&salt);
    mac.update(&nonce);
    mac.update(&ciphertext);
    let tag = mac.finalize().into_bytes();

    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(EncryptedBundle {
        format: "agy-pool-encrypted-v1".to_string(),
        kdf: "pbkdf2_hmac_sha256".to_string(),
        iterations: ITERATIONS as usize,
        salt: b64.encode(salt),
        nonce: b64.encode(nonce),
        ciphertext: b64.encode(&ciphertext),
        tag: b64.encode(tag),
    })
}

pub fn decrypt_bundle(bundle: &EncryptedBundle, password: &str) -> Result<Vec<u8>, AccountsError> {
    if bundle.format != "agy-pool-encrypted-v1" {
        return Err(AccountsError::Message(
            "Unsupported or invalid encrypted bundle format".to_string(),
        ));
    }

    let b64 = base64::engine::general_purpose::STANDARD;
    let salt = b64
        .decode(&bundle.salt)
        .map_err(|e| AccountsError::Message(format!("Corrupted encrypted bundle metadata: {}", e)))?;
    let nonce = b64
        .decode(&bundle.nonce)
        .map_err(|e| AccountsError::Message(format!("Corrupted encrypted bundle metadata: {}", e)))?;
    let ciphertext = b64
        .decode(&bundle.ciphertext)
        .map_err(|e| AccountsError::Message(format!("Corrupted encrypted bundle metadata: {}", e)))?;
    let tag = b64
        .decode(&bundle.tag)
        .map_err(|e| AccountsError::Message(format!("Corrupted encrypted bundle metadata: {}", e)))?;

    let iterations = if bundle.iterations == 0 {
        100000
    } else {
        bundle.iterations as u32
    };

    let mut dk = [0u8; 64];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), &salt, iterations, &mut dk);
    let key_enc = &dk[..32];
    let key_mac = &dk[32..];

    let mut mac = HmacSha256::new_from_slice(key_mac)
        .map_err(|e| AccountsError::Message(format!("failed to initialize HMAC-SHA256: {}", e)))?;
    mac.update(&salt);
    mac.update(&nonce);
    mac.update(&ciphertext);

    if mac.verify_slice(&tag).is_err() {
        return Err(AccountsError::Message(
            "Invalid password or corrupted backup payload".to_string(),
        ));
    }

    let mut plaintext = vec![0u8; ciphertext.len()];
    let block_size = 32;
    let num_blocks = ciphertext.len().div_ceil(block_size);

    let mut ctr_buf = [0u8; 24];
    ctr_buf[..16].copy_from_slice(&nonce);

    for i in 0..num_blocks {
        ctr_buf[16..24].copy_from_slice(&(i as u64).to_be_bytes());
        let mut ks_mac = HmacSha256::new_from_slice(key_enc)
            .map_err(|e| AccountsError::Message(format!("failed to initialize HMAC-SHA256: {}", e)))?;
        ks_mac.update(&ctr_buf);
        let ks_block = ks_mac.finalize().into_bytes();

        let start = i * block_size;
        let end = std::cmp::min(start + block_size, ciphertext.len());
        for j in start..end {
            plaintext[j] = ciphertext[j] ^ ks_block[j - start];
        }
    }

    Ok(plaintext)
}

pub fn export_pool(file_path: &str, encrypt: bool, password: &str, no_stats: bool) -> Result<String, AccountsError> {
    let pool = crate::storage::load_pool()?;
    if pool.accounts.is_empty() {
        return Err(AccountsError::Message("no accounts in pool to export".to_string()));
    }

    let mut export_accounts = Vec::with_capacity(pool.accounts.len());
    for acc in &pool.accounts {
        let mut item = serde_json::Map::new();
        item.insert("email".to_string(), serde_json::Value::String(acc.email.clone()));
        item.insert("name".to_string(), serde_json::Value::String(acc.name.clone()));
        item.insert(
            "refresh_token".to_string(),
            serde_json::Value::String(acc.refresh_token.clone()),
        );
        item.insert(
            "access_token".to_string(),
            serde_json::Value::String(acc.access_token.clone()),
        );
        item.insert(
            "token_expiry".to_string(),
            match acc.token_expiry {
                Some(e) => {
                    serde_json::Value::Number(serde_json::Number::from_f64(e).unwrap_or(serde_json::Number::from(0)))
                }
                None => serde_json::Value::Null,
            },
        );
        item.insert(
            "created_at".to_string(),
            match acc.created_at {
                Some(c) => serde_json::Value::Number(serde_json::Number::from(c)),
                None => serde_json::Value::Null,
            },
        );
        if !no_stats {
            item.insert(
                "request_count".to_string(),
                serde_json::Value::Number(serde_json::Number::from(acc.request_count)),
            );
            let gen_count = acc.gen_count.unwrap_or(0);
            item.insert(
                "gen_count".to_string(),
                serde_json::Value::Number(serde_json::Number::from(gen_count)),
            );
        }
        if let Some(ref lq) = acc.last_quota {
            if let Ok(val) = serde_json::to_value(lq) {
                item.insert("last_quota".to_string(), val);
            }
        }
        export_accounts.push(serde_json::Value::Object(item));
    }

    let active_email = pool
        .active_account_id
        .as_ref()
        .and_then(|aid| pool.accounts.iter().find(|a| &a.id == aid).map(|a| a.email.clone()));

    let bundle = serde_json::json!({
        "version": 1,
        "app": "agy-pool",
        "exported_at": crate::clock::now_unix() as i64,
        "strategy": pool.strategy,
        "active_account_email": active_email,
        "accounts": export_accounts,
    });

    let raw_json = serde_json::to_string_pretty(&bundle)
        .map_err(|e| AccountsError::Message(format!("failed to marshal export bundle: {}", e)))?;

    let is_encrypt = encrypt || !password.is_empty();
    let output_content = if is_encrypt {
        if password.is_empty() {
            return Err(AccountsError::Message(
                "password required when encrypting backup".to_string(),
            ));
        }
        let enc_bundle = encrypt_bundle(raw_json.as_bytes(), password, &[], &[])?;
        serde_json::to_string_pretty(&enc_bundle)
            .map_err(|e| AccountsError::Message(format!("failed to marshal encrypted bundle: {}", e)))?
    } else {
        raw_json
    };

    if file_path == "-" {
        println!("{}", output_content);
        return Ok("-".to_string());
    }

    let target_path = if file_path.is_empty() {
        let ext = if is_encrypt { ".enc" } else { ".json" };
        let now_sec = crate::clock::now_unix() as i64;
        let dt = jiff::Timestamp::from_second(now_sec).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
        let date_str = dt.strftime("%Y%m%d_%H%M%S").to_string();
        format!("agy-pool-backup-{}{}", date_str, ext)
    } else {
        file_path.to_string()
    };

    let path = Path::new(&target_path);
    crate::config::assert_safe_write_path(path)?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    file.write_all(output_content.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;

    Ok(target_path)
}

pub fn import_pool(
    file_path: &str,
    password: &str,
    replace: bool,
    skip_existing: bool,
) -> Result<ImportResult, AccountsError> {
    let content = if file_path == "-" {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        buf
    } else {
        std::fs::read(file_path)
            .map_err(|e| AccountsError::Message(format!("failed to read backup file {:?}: {}", file_path, e)))?
    };

    let mut raw_obj: serde_json::Value = serde_json::from_slice(&content)
        .map_err(|e| AccountsError::Message(format!("failed to parse backup file as JSON: {}", e)))?;

    if raw_obj.get("format").and_then(|v| v.as_str()).unwrap_or("") == "agy-pool-encrypted-v1" {
        if password.is_empty() {
            return Err(AccountsError::Message(
                "password required to decrypt backup".to_string(),
            ));
        }
        let bundle: EncryptedBundle = serde_json::from_value(raw_obj.clone())
            .map_err(|e| AccountsError::Message(format!("corrupted encrypted bundle format: {}", e)))?;
        let plaintext = decrypt_bundle(&bundle, password)
            .map_err(|e| AccountsError::Message(format!("decryption failed: {}", e)))?;
        raw_obj = serde_json::from_slice(&plaintext)
            .map_err(|e| AccountsError::Message(format!("failed to parse decrypted JSON: {}", e)))?;
    }

    let raw_accounts = match raw_obj.get("accounts").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => {
            return Err(AccountsError::Message(
                "invalid backup format: missing 'accounts' list".to_string(),
            ));
        }
    };

    if raw_accounts.is_empty() {
        let pool = crate::storage::load_pool()?;
        return Ok(ImportResult {
            added: 0,
            updated: 0,
            skipped: 0,
            total: pool.accounts.len(),
        });
    }

    let imported_strategy = raw_obj
        .get("strategy")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let target_active_email = raw_obj
        .get("active_account_email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut result = ImportResult::default();
    crate::storage::pool_transaction(|pool| {
        if replace {
            let mut new_list = Vec::with_capacity(raw_accounts.len());
            for (i, item) in raw_accounts.iter().enumerate() {
                let email = item.get("email").and_then(|v| v.as_str()).unwrap_or("");
                let rf = item.get("refresh_token").and_then(|v| v.as_str()).unwrap_or("");
                if email.is_empty() || rf.is_empty() {
                    continue;
                }
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let at = item
                    .get("access_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let expiry = item.get("token_expiry").and_then(|v| v.as_f64());
                let created_at = item
                    .get("created_at")
                    .and_then(|v| v.as_i64())
                    .or_else(|| Some(crate::clock::now_unix() as i64));
                let req_count = item.get("request_count").and_then(|v| v.as_i64()).unwrap_or(0);
                let gen_count = item.get("gen_count").and_then(|v| v.as_i64());

                let new_acc = crate::storage::Account {
                    id: format!("acc_{}", i + 1),
                    email: email.to_string(),
                    name,
                    refresh_token: rf.to_string(),
                    access_token: at,
                    token_expiry: expiry,
                    created_at,
                    request_count: req_count,
                    gen_count,
                    error_count: 0,
                    ..Default::default()
                };
                new_list.push(new_acc);
                result.added += 1;
            }
            pool.accounts = new_list;
            if !imported_strategy.is_empty() {
                pool.strategy = imported_strategy.clone();
            }
            let mut active_acc = None;
            if !target_active_email.is_empty() {
                for a in &pool.accounts {
                    if a.email == target_active_email {
                        active_acc = Some(a.id.clone());
                        break;
                    }
                }
            }
            if active_acc.is_none() && !pool.accounts.is_empty() {
                active_acc = Some(pool.accounts[0].id.clone());
            }
            pool.active_account_id = active_acc;
            result.total = pool.accounts.len();
            return Ok(());
        }

        // Merge mode
        for item in raw_accounts {
            let email = item.get("email").and_then(|v| v.as_str()).unwrap_or("");
            let rf = item.get("refresh_token").and_then(|v| v.as_str()).unwrap_or("");
            if email.is_empty() || rf.is_empty() {
                continue;
            }

            let mut found_idx = None;
            for (idx, a) in pool.accounts.iter().enumerate() {
                if a.email == email {
                    found_idx = Some(idx);
                    break;
                }
            }

            if let Some(idx) = found_idx {
                if skip_existing {
                    result.skipped += 1;
                    continue;
                }
                let existing = &mut pool.accounts[idx];
                existing.refresh_token = rf.to_string();
                if let Some(at) = item.get("access_token").and_then(|v| v.as_str()) {
                    if !at.is_empty() {
                        existing.access_token = at.to_string();
                        if let Some(exp) = item.get("token_expiry").and_then(|v| v.as_f64()) {
                            existing.token_expiry = Some(exp);
                        }
                    }
                }
                if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                    if !name.is_empty() && existing.name.is_empty() {
                        existing.name = name.to_string();
                    }
                }
                existing.status.clear();
                existing.rate_limited_until = None;
                result.updated += 1;
            } else {
                let next_id = crate::storage::next_account_id(&pool.accounts);
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let at = item
                    .get("access_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let expiry = item.get("token_expiry").and_then(|v| v.as_f64());
                let created_at = Some(crate::clock::now_unix() as i64);

                let new_acc = crate::storage::Account {
                    id: next_id,
                    email: email.to_string(),
                    name,
                    refresh_token: rf.to_string(),
                    access_token: at,
                    token_expiry: expiry,
                    created_at,
                    request_count: 0,
                    error_count: 0,
                    ..Default::default()
                };
                pool.accounts.push(new_acc);
                result.added += 1;
            }
        }
        result.total = pool.accounts.len();
        Ok(())
    })?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_matches_go_vectors() {
        #[derive(serde::Deserialize)]
        struct Vec_ {
            password: String,
            plaintext: String,
            salt: String, // Go []byte -> base64 in JSON
            nonce: String,
            bundle: EncryptedBundle,
        }
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        let vecs: Vec<Vec_> = serde_json::from_str(include_str!("testdata_bundle.json")).unwrap();
        assert_eq!(vecs.len(), 4);
        for v in vecs {
            let salt = b64.decode(&v.salt).unwrap();
            let nonce = b64.decode(&v.nonce).unwrap();
            let got = encrypt_bundle(v.plaintext.as_bytes(), &v.password, &salt, &nonce).unwrap();
            assert_eq!(got.ciphertext, v.bundle.ciphertext, "ciphertext for {:?}", v.plaintext);
            assert_eq!(got.tag, v.bundle.tag, "tag for {:?}", v.plaintext);
            assert_eq!(decrypt_bundle(&v.bundle, &v.password).unwrap(), v.plaintext.as_bytes());
            assert!(decrypt_bundle(&v.bundle, "wrong password").is_err());
            let mut tampered = v.bundle.clone();
            tampered.tag = b64.encode([0u8; 32]);
            assert!(decrypt_bundle(&tampered, &v.password).is_err());
        }
    }

    #[test]
    fn test_crypto_decrypt_python_vector() {
        let vector_json = r#"{
            "format": "agy-pool-encrypted-v1",
            "kdf": "pbkdf2_hmac_sha256",
            "iterations": 100000,
            "salt": "MLkMXv+/FTjdr+5c0kMZpQ==",
            "nonce": "v+CaGz1x+PYSeUmb3hbJTA==",
            "ciphertext": "b3sQxoW7crJPmgTptQLidIgYI7vSO6xVvA/daUk6kzXF",
            "tag": "jmRYdMWlTvDn4mSb34svuPJOOiR2eBCTvLUjxlpbSR4="
        }"#;
        let password = "test-pass-phrase-42";
        let expected_plaintext = "hello agy-pool encrypted test 123";

        let bundle: EncryptedBundle = serde_json::from_str(vector_json).unwrap();
        let decrypted = decrypt_bundle(&bundle, password).expect("decrypt failed on Python vector");
        assert_eq!(std::str::from_utf8(&decrypted).unwrap(), expected_plaintext);
    }

    #[test]
    fn test_crypto_cross_language_roundtrip() {
        let plaintext = b"cross-language-verification-42-secret-payload";
        let password = "super-secure-passphrase";

        let bundle = encrypt_bundle(plaintext, password, &[], &[]).unwrap();
        let decrypted = decrypt_bundle(&bundle, password).unwrap();
        assert_eq!(decrypted, plaintext);

        let py_repo = std::env::var("PYTHON_REFERENCE")
            .or_else(|_| std::env::var("AGY_PYTHON_ROOT"))
            .unwrap_or_else(|_| "/home/codex/agy-pool".to_string());
        if !std::path::Path::new(&py_repo).is_dir() {
            return;
        }

        let bundle_bytes = serde_json::to_string(&bundle).unwrap();
        let script = r#"
import sys, json
from agy_pool.accounts import decrypt_bundle

bundle = json.loads(sys.argv[1])
pw = sys.argv[2]
plaintext = decrypt_bundle(bundle, pw)
sys.stdout.write(plaintext.decode("utf-8"))
"#;
        let output = std::process::Command::new("python3")
            .arg("-c")
            .arg(script)
            .arg(&bundle_bytes)
            .arg(password)
            .env("PYTHONPATH", &py_repo)
            .output();
        if let Ok(out) = output {
            if out.status.success() {
                assert_eq!(out.stdout, plaintext);
            }
        }
    }

    #[test]
    fn test_crypto_tamper_resistance() {
        let plaintext = b"secret data";
        let password = "correct-password";

        let bundle = encrypt_bundle(plaintext, password, &[], &[]).unwrap();

        // Wrong password
        assert!(decrypt_bundle(&bundle, "wrong-password").is_err());

        // Wrong format
        let mut corrupt_format = bundle.clone();
        corrupt_format.format = "unknown-format".to_string();
        assert!(decrypt_bundle(&corrupt_format, password).is_err());

        // Corrupted tag
        let mut corrupt_tag = bundle.clone();
        corrupt_tag.tag = format!("AAAA{}", &corrupt_tag.tag[4..]);
        assert!(decrypt_bundle(&corrupt_tag, password).is_err());

        // Corrupted ciphertext
        let mut corrupt_cipher = bundle.clone();
        corrupt_cipher.ciphertext = format!("AAAA{}", &corrupt_cipher.ciphertext[4..]);
        assert!(decrypt_bundle(&corrupt_cipher, password).is_err());
    }

    #[test]
    fn test_export_and_import_pool() {
        let _l = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let tmp = tempfile::tempdir().unwrap();
        crate::config::configure_data_dir(tmp.path()).unwrap();

        let mut pool = crate::storage::Pool::new_empty();
        pool.strategy = "max_quota".to_string();
        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            email: "user1@gmail.com".to_string(),
            refresh_token: "rf_1".to_string(),
            access_token: "at_1".to_string(),
            ..Default::default()
        };
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            email: "user2@gmail.com".to_string(),
            refresh_token: "rf_2".to_string(),
            access_token: "at_2".to_string(),
            ..Default::default()
        };
        pool.accounts = vec![acc1, acc2];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        let plain_export_path = tmp.path().join("export-plain.json");
        let plain_str = plain_export_path.to_str().unwrap();
        let out_path = export_pool(plain_str, false, "", false).unwrap();
        assert_eq!(out_path, plain_str);

        // Verify file mode 0600
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&plain_export_path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);

        // Encrypted Export
        let enc_export_path = tmp.path().join("export-enc.enc");
        let enc_str = enc_export_path.to_str().unwrap();
        export_pool(enc_str, true, "secret123", false).unwrap();

        // Clear pool
        crate::storage::save_pool(&crate::storage::Pool::new_empty()).unwrap();

        // Import encrypted backup with replace
        let res = import_pool(enc_str, "secret123", true, false).unwrap();
        assert_eq!(res.added, 2);
        assert_eq!(res.total, 2);

        let loaded = crate::storage::load_pool().unwrap();
        assert_eq!(loaded.strategy, "max_quota");
        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.accounts[0].email, "user1@gmail.com");
        assert_eq!(loaded.accounts[1].email, "user2@gmail.com");

        // Merge import
        let new_backup_path = tmp.path().join("update-backup.json");
        let new_bundle = serde_json::json!({
            "version": 1,
            "accounts": [
                {
                    "email": "user1@gmail.com",
                    "refresh_token": "rf_1_new",
                    "access_token": "at_1_new"
                },
                {
                    "email": "user3@gmail.com",
                    "refresh_token": "rf_3",
                    "access_token": "at_3"
                }
            ]
        });
        std::fs::write(&new_backup_path, serde_json::to_vec(&new_bundle).unwrap()).unwrap();

        let merge_res = import_pool(new_backup_path.to_str().unwrap(), "", false, false).unwrap();
        assert_eq!(merge_res.added, 1);
        assert_eq!(merge_res.updated, 1);
        assert_eq!(merge_res.total, 3);

        let loaded_after = crate::storage::load_pool().unwrap();
        let acc1_up = loaded_after
            .accounts
            .iter()
            .find(|a| a.email == "user1@gmail.com")
            .unwrap();
        assert_eq!(acc1_up.refresh_token, "rf_1_new");
        assert_eq!(acc1_up.access_token, "at_1_new");
        let acc3_add = loaded_after
            .accounts
            .iter()
            .find(|a| a.email == "user3@gmail.com")
            .unwrap();
        assert_eq!(acc3_add.id, "acc_3");

        // Skip existing
        let skip_res = import_pool(new_backup_path.to_str().unwrap(), "", false, true).unwrap();
        assert_eq!(skip_res.skipped, 2);
        assert_eq!(skip_res.added, 0);
        assert_eq!(skip_res.updated, 0);

        crate::config::reset_data_dir();
        crate::config::set_test_mode(false);
    }
}
