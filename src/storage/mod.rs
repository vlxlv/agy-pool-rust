pub mod db;
pub mod inflight;
pub mod model;
pub mod pool;

pub use db::*;
pub use inflight::{
    LeaseGuard, lease_in_flight, lease_in_flight_with_db, reserve_in_flight, reserve_in_flight_with_db,
};
pub use model::*;
pub use pool::*;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use super::*;

    struct TestEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            crate::config::reset_data_dir();
            crate::config::set_test_mode(false);
        }
    }

    fn setup_test_env() -> TestEnv {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let dir = tempfile::tempdir().unwrap();
        crate::config::configure_data_dir(dir.path()).unwrap();
        let path = dir.path().to_path_buf();
        TestEnv {
            _lock: lock,
            _dir: dir,
            path,
        }
    }

    #[test]
    fn unknown_extra_keys_round_trip() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let dir = tempfile::tempdir().unwrap();
        let db = StateDb::open(&dir.path().join("state.db")).unwrap();
        db.update(|tx| {
            tx.execute(
                "INSERT INTO accounts (id, name, email, extra) VALUES ('acc_1', 'Main', 'a@b', ?1)",
                [r#"{"future_field":{"x":1},"note":"keep me"}"#],
            )?;
            tx.execute(
                "INSERT INTO account_quota (account_id, gemini_5h_fraction, raw_extra) VALUES ('acc_1', 0.5, ?1)",
                [r#"{"vendor_bucket":[1,2,3]}"#],
            )?;
            Ok(())
        })
        .unwrap();
        let pool = db.view(load_pool_from_tx).unwrap();
        db.update(|tx| save_pool_to_tx(tx, &pool)).unwrap();
        let (extra, raw): (String, String) = db
            .view(|tx| {
                Ok(tx.query_row(
                    "SELECT a.extra, q.raw_extra FROM accounts a JOIN account_quota q ON q.account_id = a.id",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        let extra: serde_json::Value = serde_json::from_str(&extra).unwrap();
        let raw: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(extra["future_field"]["x"], 1);
        assert_eq!(extra["note"], "keep me");
        assert_eq!(raw["vendor_bucket"], serde_json::json!([1, 2, 3]));
        crate::config::set_test_mode(false);
    }

    #[test]
    fn test_open_state_db_creation_and_permissions() {
        let _env = setup_test_env();
        let tmp_dir = tempfile::tempdir().unwrap();
        let db_path = tmp_dir.path().join("state.db");

        let sdb = StateDb::open(&db_path).unwrap();

        let meta = std::fs::metadata(&db_path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);

        let schema_ver: String = sdb
            .view(|tx| {
                tx.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0))
                    .map_err(StorageError::Sqlite)
            })
            .unwrap();
        assert_eq!(schema_ver, STATE_DB_SCHEMA_VERSION);

        let wal_path = format!("{}-wal", db_path.display());
        if let Ok(wmeta) = std::fs::metadata(&wal_path) {
            assert_eq!(wmeta.permissions().mode() & 0o777, 0o600);
        }
        let shm_path = format!("{}-shm", db_path.display());
        if let Ok(smeta) = std::fs::metadata(&shm_path) {
            assert_eq!(smeta.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn test_open_state_db_zero_bytes_refusal() {
        let _env = setup_test_env();
        let tmp_dir = tempfile::tempdir().unwrap();
        let db_path = tmp_dir.path().join("state.db");

        std::fs::write(&db_path, b"").unwrap();
        assert!(StateDb::open(&db_path).is_err());

        let meta = std::fs::metadata(&db_path).unwrap();
        assert_eq!(meta.len(), 0);
    }

    #[test]
    fn test_open_state_db_corrupt_refusal() {
        let _env = setup_test_env();
        let tmp_dir = tempfile::tempdir().unwrap();
        let db_path = tmp_dir.path().join("state.db");

        std::fs::write(&db_path, b"NOT A SQLITE FILE HEADER").unwrap();
        assert!(StateDb::open(&db_path).is_err());

        let content = std::fs::read(&db_path).unwrap();
        assert_eq!(content, b"NOT A SQLITE FILE HEADER");
    }

    #[test]
    fn test_open_state_db_unsupported_version_fails_closed() {
        let _env = setup_test_env();
        let tmp_dir = tempfile::tempdir().unwrap();
        let db_path = tmp_dir.path().join("state.db");

        let sdb = StateDb::open(&db_path).unwrap();
        sdb.update(|tx| {
            tx.execute("UPDATE meta SET value = '99' WHERE key = 'schema_version'", [])?;
            Ok(())
        })
        .unwrap();
        sdb.close().unwrap();

        assert!(StateDb::open(&db_path).is_err());
    }

    #[test]
    fn test_cascade_deletion() {
        let _env = setup_test_env();
        let mut p = Pool::new_empty();
        p.accounts.push(Account {
            id: "acc_1".to_string(),
            email: "user@example.com".to_string(),
            request_count: 10,
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.0),
                    reset_time: None,
                    extra: serde_json::Map::new(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        save_pool(&p).unwrap();

        let sdb = StateDb::global().unwrap();
        let (rt_count, q_count): (i64, i64) = sdb
            .view(|tx| {
                let rt: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM account_runtime WHERE account_id = 'acc_1'",
                    [],
                    |r| r.get(0),
                )?;
                let q: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM account_quota WHERE account_id = 'acc_1'",
                    [],
                    |r| r.get(0),
                )?;
                Ok((rt, q))
            })
            .unwrap();
        assert_eq!(rt_count, 1);
        assert_eq!(q_count, 1);

        sdb.update(|tx| {
            tx.execute("DELETE FROM accounts WHERE id = 'acc_1'", [])?;
            Ok(())
        })
        .unwrap();

        let (rt_count_after, q_count_after): (i64, i64) = sdb
            .view(|tx| {
                let rt: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM account_runtime WHERE account_id = 'acc_1'",
                    [],
                    |r| r.get(0),
                )?;
                let q: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM account_quota WHERE account_id = 'acc_1'",
                    [],
                    |r| r.get(0),
                )?;
                Ok((rt, q))
            })
            .unwrap();
        assert_eq!(rt_count_after, 0);
        assert_eq!(q_count_after, 0);
    }

    #[test]
    fn test_checkpoint_passive_bounds_wal() {
        let _env = setup_test_env();
        let tmp_dir = tempfile::tempdir().unwrap();
        let db_path = tmp_dir.path().join("state.db");
        let sdb = StateDb::open(&db_path).unwrap();

        let put = |val: &str| {
            sdb.update(|tx| {
                tx.execute(
                    "INSERT OR REPLACE INTO meta (key, value) VALUES ('wal_test', ?1)",
                    [val],
                )?;
                Ok(())
            })
            .unwrap();
        };

        let big = vec![b'x'; 3 << 20];
        let big_str = String::from_utf8(big).unwrap();
        put(&big_str);
        put("");
        sdb.checkpoint_passive().unwrap();
        put("small");

        let wal_path = format!("{}-wal", db_path.display());
        let fi = std::fs::metadata(&wal_path).unwrap();
        assert!(fi.len() <= 1 << 20, "WAL not truncated: {} bytes", fi.len());
    }

    #[test]
    fn test_state_db_serializable_immediate_concurrency() {
        let _env = setup_test_env();
        let tmp_dir = tempfile::tempdir().unwrap();
        let db_path = tmp_dir.path().join("state.db");
        let sdb = Arc::new(StateDb::open(&db_path).unwrap());

        sdb.update(|tx| {
            tx.execute("INSERT INTO runtime_global (key, value) VALUES ('counter', '0')", [])?;
            Ok(())
        })
        .unwrap();

        const GOROUTINES: usize = 20;
        const INCREMENTS_PER_GOROUTINE: usize = 25;
        const EXPECTED_FINAL: i64 = (GOROUTINES * INCREMENTS_PER_GOROUTINE) as i64;

        let mut handles = Vec::new();
        for _ in 0..GOROUTINES {
            let sdb_clone = sdb.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..INCREMENTS_PER_GOROUTINE {
                    sdb_clone
                        .update(|tx| {
                            let val: i64 = tx.query_row(
                                "SELECT CAST(value AS INTEGER) FROM runtime_global WHERE key = 'counter'",
                                [],
                                |r| r.get(0),
                            )?;
                            let next_val = format!("{}", val + 1);
                            tx.execute(
                                "UPDATE runtime_global SET value = ?1 WHERE key = 'counter'",
                                [&next_val],
                            )?;
                            Ok(())
                        })
                        .unwrap();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let final_val: i64 = sdb
            .view(|tx| {
                let val: i64 = tx.query_row(
                    "SELECT CAST(value AS INTEGER) FROM runtime_global WHERE key = 'counter'",
                    [],
                    |r| r.get(0),
                )?;
                Ok(val)
            })
            .unwrap();

        assert_eq!(final_val, EXPECTED_FINAL);
    }

    #[test]
    fn test_state_db_multi_process_simulation() {
        let _env = setup_test_env();
        let tmp_dir = tempfile::tempdir().unwrap();
        let db_path = tmp_dir.path().join("state.db");

        let proc_a = StateDb::open(&db_path).unwrap();
        let mut pool = Pool::new_empty();
        pool.accounts.push(Account {
            id: "acc_1".to_string(),
            email: "proc@example.com".to_string(),
            ..Default::default()
        });
        proc_a.update(|tx| save_pool_to_tx(tx, &pool)).unwrap();

        let proc_b = StateDb::open(&db_path).unwrap();
        let proc_c = StateDb::open(&db_path).unwrap();

        let h_a = std::thread::spawn(move || {
            for _ in 0..20 {
                proc_a
                    .update(|tx| {
                        tx.execute(
                            "UPDATE account_runtime SET request_count = request_count + 1 WHERE account_id = 'acc_1'",
                            [],
                        )?;
                        Ok(())
                    })
                    .unwrap();
            }
        });

        let h_b = std::thread::spawn(move || {
            for _ in 0..20 {
                let _cnt: i64 = proc_b
                    .view(|tx| {
                        let cnt: i64 = tx.query_row(
                            "SELECT request_count FROM account_runtime WHERE account_id = 'acc_1'",
                            [],
                            |r| r.get(0),
                        )?;
                        Ok(cnt)
                    })
                    .unwrap();
            }
        });

        let h_c = std::thread::spawn(move || {
            for _ in 0..20 {
                proc_c
                    .update(|tx| {
                        tx.execute(
                            "UPDATE account_runtime SET error_count = error_count + 1 WHERE account_id = 'acc_1'",
                            [],
                        )?;
                        Ok(())
                    })
                    .unwrap();
            }
        });

        h_a.join().unwrap();
        h_b.join().unwrap();
        h_c.join().unwrap();

        let sdb = StateDb::open(&db_path).unwrap();
        let (req, err): (i64, i64) = sdb
            .view(|tx| {
                let req: i64 = tx.query_row(
                    "SELECT request_count FROM account_runtime WHERE account_id = 'acc_1'",
                    [],
                    |r| r.get(0),
                )?;
                let err: i64 = tx.query_row(
                    "SELECT error_count FROM account_runtime WHERE account_id = 'acc_1'",
                    [],
                    |r| r.get(0),
                )?;
                Ok((req, err))
            })
            .unwrap();

        assert_eq!(req, 20);
        assert_eq!(err, 20);
    }

    #[test]
    fn test_migration_rollback_on_failure() {
        let _env = setup_test_env();
        let tmp_dir = tempfile::tempdir().unwrap();
        let target_dir = tmp_dir.path().join("target");
        let malformed_file = tmp_dir.path().join("corrupt.json");
        std::fs::write(&malformed_file, b"NOT JSON AT ALL").unwrap();

        assert!(migrate_legacy_pool(&malformed_file, &target_dir).is_err());

        let target_db = target_dir.join("state.db");
        assert!(!target_db.exists());

        if let Ok(entries) = std::fs::read_dir(&target_dir)
            && let Some(entry) = entries.flatten().next()
        {
            panic!("unexpected leftover file in target directory: {:?}", entry.file_name());
        }
    }

    #[test]
    fn test_load_golden_pool_fixtures() {
        let testdata_dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/pool/"));

        // 1. Empty pool
        let empty_bytes = std::fs::read(testdata_dir.join("empty.json")).unwrap();
        let empty_pool: Pool = serde_json::from_slice(&empty_bytes).unwrap();
        assert_eq!(empty_pool.accounts.len(), 0);
        assert_eq!(empty_pool.version, 1);
        assert_eq!(empty_pool.strategy, "max_quota");

        // 2. Valid 3 accounts
        let valid_bytes = std::fs::read(testdata_dir.join("valid_3_accounts.json")).unwrap();
        let valid_pool: Pool = serde_json::from_slice(&valid_bytes).unwrap();
        assert_eq!(valid_pool.accounts.len(), 3);
        assert_eq!(valid_pool.active_account_id.as_deref(), Some("acc_1"));

        // 3. Legacy alpha.9 pool
        let legacy_bytes = std::fs::read(testdata_dir.join("legacy_alpha9.json")).unwrap();
        let legacy_pool: Pool = serde_json::from_slice(&legacy_bytes).unwrap();
        assert_eq!(legacy_pool.accounts.len(), 1);
        assert_eq!(
            legacy_pool.accounts[0]
                .last_quota
                .as_ref()
                .and_then(|q| q.remaining_fraction),
            Some(0.65)
        );

        // 4. Corrupt syntax returns error
        let corrupt_bytes = std::fs::read(testdata_dir.join("corrupt.json")).unwrap();
        assert!(serde_json::from_slice::<Pool>(&corrupt_bytes).is_err());
    }

    #[test]
    fn test_observability_case_l_persistence_failure() {
        let _env = setup_test_env();
        crate::observability::reset();

        let err = pool_transaction(|_pool| Err(StorageError::Other("simulated transaction failure".to_string())));
        assert!(err.is_err());

        let snap = crate::observability::get_snapshot();
        assert_eq!(snap.persistence_errors, 1);

        let _ = pool_transaction(|_pool| Err(StorageError::Other("simulated transaction failure".to_string())));

        let snap2 = crate::observability::get_snapshot();
        assert_eq!(snap2.persistence_errors, 2);
    }

    #[test]
    fn test_record_success() {
        let _env = setup_test_env();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let past_cooldown = (now - 100) as f64;
        let future_cooldown = (now + 3600) as f64;

        let acc1 = Account {
            id: "acc_1".to_string(),
            status: STATUS_VERIFICATION_PENDING.to_string(),
            rate_limited_until: Some(past_cooldown),
            gen_count: None,
            ..Default::default()
        };
        let acc2 = Account {
            id: "acc_2".to_string(),
            rate_limited_until: Some(future_cooldown),
            ..Default::default()
        };

        let mut pool = Pool::new_empty();
        pool.active_account_id = Some("acc_2".to_string());
        pool.accounts = vec![acc1, acc2];
        save_pool(&pool).unwrap();

        // 1. Generation success on acc_1: promotes to active, clears pending status and past cooldown
        record_success(
            &Account {
                id: "acc_1".to_string(),
                ..Default::default()
            },
            true,
            now,
        )
        .unwrap();

        let reloaded = load_pool().unwrap();
        let r_acc1 = find_account(
            &reloaded,
            &Account {
                id: "acc_1".to_string(),
                ..Default::default()
            },
        )
        .expect("acc_1 not found in reloaded pool");
        assert_eq!(r_acc1.request_count, 1);
        assert_eq!(r_acc1.gen_count, Some(1));
        assert_eq!(r_acc1.last_used_at, Some(now));
        assert_eq!(r_acc1.status, "");
        assert_eq!(r_acc1.rate_limited_until, None);

        assert_eq!(reloaded.generation_events.len(), 1);
        assert_eq!(reloaded.generation_events[0].id, "acc_1");
        assert_eq!(reloaded.active_account_id.as_deref(), Some("acc_1"));

        let r_acc2 = find_account(
            &reloaded,
            &Account {
                id: "acc_2".to_string(),
                ..Default::default()
            },
        )
        .expect("acc_2 not found in reloaded pool");
        assert_eq!(r_acc2.rate_limited_until, Some(future_cooldown));
        assert_eq!(r_acc2.request_count, 0);

        // 2. Non-generation success on validation_required account: only bumps request_count
        let mut reloaded = reloaded;
        let validation_acc = Account {
            id: "acc_3".to_string(),
            status: STATUS_VALIDATION_REQUIRED.to_string(),
            rate_limited_until: Some(future_cooldown),
            ..Default::default()
        };
        reloaded.accounts.push(validation_acc);
        save_pool(&reloaded).unwrap();

        record_success(
            &Account {
                id: "acc_3".to_string(),
                ..Default::default()
            },
            false,
            now,
        )
        .unwrap();

        let reloaded2 = load_pool().unwrap();
        let r_acc3 = find_account(
            &reloaded2,
            &Account {
                id: "acc_3".to_string(),
                ..Default::default()
            },
        )
        .expect("acc_3 not found in reloaded2 pool");
        assert_eq!(r_acc3.request_count, 1);
        assert_eq!(r_acc3.status, STATUS_VALIDATION_REQUIRED);
        assert_eq!(r_acc3.rate_limited_until, Some(future_cooldown));
        assert_eq!(r_acc3.gen_count, None);
        assert_eq!(r_acc3.last_used_at, None);
    }

    #[test]
    fn test_load_missing_file_returns_empty() {
        let _env = setup_test_env();
        let pool = load_pool().unwrap();
        assert_eq!(pool.version, 1);
        assert_eq!(pool.strategy, "smart");
        assert_eq!(pool.accounts.len(), 0);
    }

    #[test]
    fn test_save_and_load_roundtrip_and_permissions() {
        let _env = setup_test_env();
        let mut p = Pool::new_empty();
        p.strategy = "max_quota".to_string();
        p.accounts.push(Account {
            id: "acc_1".to_string(),
            name: "First".to_string(),
            request_count: 42,
            ..Default::default()
        });

        save_pool(&p).unwrap();

        let state_db_file = crate::config::state_db_file();
        let info = std::fs::metadata(&state_db_file).unwrap();
        assert_eq!(info.permissions().mode() & 0o777, 0o600);

        let loaded = load_pool().unwrap();
        assert_eq!(loaded.strategy, "max_quota");
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(loaded.accounts[0].id, "acc_1");
        assert_eq!(loaded.accounts[0].request_count, 42);
    }

    #[test]
    fn test_corrupt_file_returns_error() {
        let _env = setup_test_env();
        let state_db_file = crate::config::state_db_file();
        std::fs::write(&state_db_file, b"{not valid sqlite database").unwrap();

        assert!(load_pool().is_err());

        let tx_err = pool_transaction(|pool| {
            pool.strategy = "smart".to_string();
            Ok(())
        });
        assert!(tx_err.is_err());

        let raw = std::fs::read(&state_db_file).unwrap();
        assert_eq!(raw, b"{not valid sqlite database");
    }

    #[test]
    fn test_failed_transaction_aborts() {
        let _env = setup_test_env();
        let mut initial = Pool::new_empty();
        initial.strategy = "max_quota".to_string();
        save_pool(&initial).unwrap();

        let err = pool_transaction(|pool| {
            pool.strategy = "smart".to_string();
            Err(StorageError::Other("simulated business error".to_string()))
        });
        assert!(err.is_err());

        let loaded = load_pool().unwrap();
        assert_eq!(loaded.strategy, "max_quota");
    }

    #[test]
    fn test_temp_files_cleaned_up() {
        let env = setup_test_env();
        let p = Pool::new_empty();
        save_pool(&p).unwrap();

        let entries = std::fs::read_dir(&env.path).unwrap();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(!name.ends_with(".tmp"), "found leftover temp file: {}", name);
        }
    }

    #[test]
    fn test_concurrent_goroutines_no_lost_updates() {
        let _env = setup_test_env();
        let mut p = Pool::new_empty();
        p.accounts.push(Account {
            id: "acc_counter".to_string(),
            request_count: 0,
            ..Default::default()
        });
        save_pool(&p).unwrap();

        const CONCURRENCY: usize = 20;
        let mut handles = Vec::new();
        for _ in 0..CONCURRENCY {
            handles.push(std::thread::spawn(|| {
                pool_transaction(|pool| {
                    for a in &mut pool.accounts {
                        if a.id == "acc_counter" {
                            a.request_count += 1;
                        }
                    }
                    Ok(())
                })
                .unwrap();
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let loaded = load_pool().unwrap();
        assert_eq!(loaded.accounts[0].request_count, CONCURRENCY as i64);
    }

    #[test]
    fn test_find_account_and_next_id() {
        let pool = Pool {
            accounts: vec![
                Account {
                    id: "acc_1".to_string(),
                    email: "alpha@example.com".to_string(),
                    ..Default::default()
                },
                Account {
                    id: "acc_3".to_string(),
                    email: "gamma@example.com".to_string(),
                    ..Default::default()
                },
            ],
            ..Pool::new_empty()
        };

        let found = find_account(
            &pool,
            &Account {
                id: "acc_1".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(found.unwrap().email, "alpha@example.com");

        let found_email = find_account(
            &pool,
            &Account {
                email: "gamma@example.com".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(found_email.unwrap().id, "acc_3");

        let next = next_account_id(&pool.accounts);
        assert_eq!(next, "acc_2");

        let mut accounts = pool.accounts.clone();
        accounts.push(Account {
            id: "acc_2".to_string(),
            ..Default::default()
        });
        let next2 = next_account_id(&accounts);
        assert_eq!(next2, "acc_4");
    }

    #[test]
    fn test_migrate_legacy_pool() {
        let _env = setup_test_env();
        let src_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();

        let legacy_file = src_dir.path().join("agy-pool-accounts.json");
        let legacy_content = r#"{
  "version": 1,
  "strategy": "least_used",
  "active_account_id": "acc_1",
  "accounts": [
    {
      "id": "acc_1",
      "email": "user1@example.com",
      "access_token": "token1",
      "refresh_token": "refresh1",
      "request_count": 5
    },
    {
      "id": "acc_2",
      "email": "user2@example.com",
      "access_token": "token2",
      "refresh_token": "refresh2",
      "request_count": 10
    }
  ]
}"#;
        std::fs::write(&legacy_file, legacy_content).unwrap();

        let src_hash_before = crate::config::compute_config_file_hash(&legacy_file).unwrap();

        let res = migrate_legacy_pool(&legacy_file, target_dir.path()).unwrap();
        assert_eq!(res.accounts_count, 2);
        assert_eq!(res.active_account_id, "acc_1");
        assert_eq!(res.strategy, "smart");

        let src_hash_after = crate::config::compute_config_file_hash(&legacy_file).unwrap();
        assert_eq!(src_hash_before, src_hash_after);

        let target_db = target_dir.path().join("state.db");
        let sdb = StateDb::open(&target_db).unwrap();
        let db_pool = sdb.view(load_pool_from_tx).unwrap();
        assert_eq!(db_pool.accounts.len(), 2);
        assert_eq!(db_pool.accounts[0].id, "acc_1");
        assert_eq!(db_pool.accounts[1].id, "acc_2");
        assert_eq!(db_pool.active_account_id.as_deref(), Some("acc_1"));
        assert_eq!(db_pool.strategy, "smart");

        let target_accounts = target_dir.path().join("accounts.json");
        let loaded = read_pool_unlocked(&target_accounts).unwrap();
        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.accounts[0].id, "acc_1");
        assert_eq!(loaded.accounts[1].id, "acc_2");
        assert_eq!(loaded.strategy, "smart");

        let prod_dir = crate::config::real_production_gemini_dir();
        assert!(migrate_legacy_pool(&legacy_file, &prod_dir).is_err());
    }

    #[test]
    fn test_migrate_split_json_to_db() {
        let _env = setup_test_env();
        let src_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();

        let accs = r#"{
		"version": 1,
		"accounts": [
			{
				"id": "acc_split1",
				"email": "user1@example.com",
				"access_token": "tok1",
				"refresh_token": "ref1"
			}
		]
	}"#;
        std::fs::write(src_dir.path().join("accounts.json"), accs).unwrap();

        let mut rt_accounts = HashMap::new();
        rt_accounts.insert(
            "acc_split1".to_string(),
            TargetAccountRuntimeRecord {
                status: "active".to_string(),
                request_count: 99,
                error_count: 3,
                ..Default::default()
            },
        );
        let rt = TargetRuntimeFile {
            version: 1,
            active_account_id: Some("acc_split1".to_string()),
            accounts: rt_accounts,
        };
        std::fs::write(src_dir.path().join("runtime.json"), serde_json::to_vec(&rt).unwrap()).unwrap();

        let mut q_accounts = HashMap::new();
        q_accounts.insert(
            "acc_split1".to_string(),
            QuotaState {
                remaining_fraction: Some(0.75),
                updated_at: Some(1234567),
                ..Default::default()
            },
        );
        let q = TargetQuotaFile {
            version: 1,
            accounts: q_accounts,
            updated_at: None,
        };
        std::fs::write(src_dir.path().join("quota.json"), serde_json::to_vec(&q).unwrap()).unwrap();

        let res = migrate_split_json_to_db(src_dir.path(), target_dir.path()).unwrap();
        assert_eq!(res.accounts_count, 1);
        assert_eq!(res.active_account_id, "acc_split1");

        let target_db = target_dir.path().join("state.db");
        let sdb = StateDb::open(&target_db).unwrap();
        sdb.view(|tx| {
            let p = load_pool_from_tx(tx)?;
            assert_eq!(p.accounts.len(), 1);
            let a = &p.accounts[0];
            assert_eq!(a.id, "acc_split1");
            assert_eq!(a.request_count, 99);
            assert_eq!(a.error_count, 3);
            assert_eq!(a.last_quota.as_ref().and_then(|q| q.remaining_fraction), Some(0.75));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_target_schemas() {
        let acc = TargetAccountAuthRecord {
            id: "acc_1".to_string(),
            email: "test@example.com".to_string(),
            access_token: "ya29.test".to_string(),
            ..Default::default()
        };
        let accs = TargetAccountsFile {
            version: 1,
            accounts: vec![acc],
        };
        let data = serde_json::to_string(&accs).unwrap();
        let parsed: TargetAccountsFile = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed.accounts.len(), 1);
        assert_eq!(parsed.accounts[0].email, "test@example.com");

        let mut accounts_runtime = HashMap::new();
        accounts_runtime.insert(
            "acc_1".to_string(),
            TargetAccountRuntimeRecord {
                request_count: 42,
                status: "active".to_string(),
                ..Default::default()
            },
        );
        let rt = TargetRuntimeFile {
            version: 1,
            active_account_id: None,
            accounts: accounts_runtime,
        };
        let rt_data = serde_json::to_string(&rt).unwrap();
        let parsed_rt: TargetRuntimeFile = serde_json::from_str(&rt_data).unwrap();
        assert_eq!(parsed_rt.accounts["acc_1"].request_count, 42);
    }

    #[test]
    fn test_empty_pool_roundtrip() {
        let empty_json = r#"{"version":1,"strategy":"max_quota","active_account_id":null,"accounts":[]}"#;
        let p: Pool = serde_json::from_str(empty_json).unwrap();
        assert_eq!(p.version, 1);
        assert_eq!(p.strategy, "max_quota");
        assert_eq!(p.active_account_id, None);
        assert_eq!(p.accounts.len(), 0);

        let encoded = serde_json::to_string(&p).unwrap();
        assert_eq!(encoded, empty_json);
    }

    #[test]
    fn test_unknown_field_preservation() {
        let raw_json = r#"{
  "version": 1,
  "strategy": "max_quota",
  "active_account_id": "acc_1",
  "custom_root_field": "preserved_root_value",
  "accounts": [
    {
      "id": "acc_1",
      "name": "Primary",
      "email": "user@example.com",
      "request_count": 5,
      "gen_count": 2,
      "error_count": 0,
      "custom_account_field": 12345,
      "last_quota": {
        "gemini_5h": {
          "fraction": 0.85,
          "custom_window_field": true
        }
      }
    }
  ]
}"#;
        let p: Pool = serde_json::from_str(raw_json).unwrap();
        assert_eq!(p.active_account_id.as_deref(), Some("acc_1"));
        assert_eq!(p.accounts.len(), 1);
        let acc = &p.accounts[0];
        assert_eq!(acc.id, "acc_1");
        assert_eq!(acc.name, "Primary");
        assert_eq!(acc.request_count, 5);

        assert_eq!(
            p.extra.get("custom_root_field"),
            Some(&serde_json::Value::String("preserved_root_value".to_string()))
        );
        assert_eq!(acc.extra.get("custom_account_field"), Some(&serde_json::json!(12345)));
        assert_eq!(
            acc.last_quota
                .as_ref()
                .unwrap()
                .gemini_5h
                .as_ref()
                .unwrap()
                .extra
                .get("custom_window_field"),
            Some(&serde_json::json!(true))
        );

        let reencoded = serde_json::to_string(&p).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&reencoded).unwrap();
        assert_eq!(parsed["custom_root_field"], "preserved_root_value");
        assert_eq!(parsed["accounts"][0]["custom_account_field"], 12345);
        assert_eq!(
            parsed["accounts"][0]["last_quota"]["gemini_5h"]["custom_window_field"],
            true
        );
    }

    #[test]
    fn test_invalid_pool_schema_fails() {
        let invalid = r#"{"version": 1, "strategy": "max_quota", "accounts": "invalid"}"#;
        assert!(serde_json::from_str::<Pool>(invalid).is_err());
    }

    #[test]
    fn test_legacy_fields_decoded() {
        let legacy_json = r#"{
  "version": 1,
  "strategy": "max_quota",
  "accounts": [
    {
      "id": "acc_legacy",
      "quota": 0.75,
      "gemini_5h_pct": 80.0,
      "gemini_weekly_pct": 90.0,
      "gemini_5h_reset_sec": 3600.0,
      "request_count": 10,
      "gen_count": 4,
      "error_count": 0
    }
  ]
}"#;
        let p: Pool = serde_json::from_str(legacy_json).unwrap();
        let acc = &p.accounts[0];
        assert_eq!(acc.quota, Some(0.75));
        assert_eq!(acc.gemini_5h_pct, Some(80.0));
        assert_eq!(acc.gemini_5h_reset_sec, Some(3600.0));
    }

    #[test]
    fn test_absent_vs_zero() {
        let data = r#"{"id":"acc_1","request_count":0,"gen_count":0,"error_count":0}"#;
        let acc: Account = serde_json::from_str(data).unwrap();
        assert_eq!(acc.rate_limited_until, None);
        assert_eq!(acc.last_used_at, None);
        assert_eq!(acc.token_expiry, None);

        let data_zero = r#"{"id":"acc_1","rate_limited_until":0.0,"request_count":0,"gen_count":0,"error_count":0}"#;
        let acc_zero: Account = serde_json::from_str(data_zero).unwrap();
        assert_eq!(acc_zero.rate_limited_until, Some(0.0));
    }

    #[test]
    fn test_load_and_save_runtime_value() {
        let _env = setup_test_env();
        assert_eq!(load_runtime_value("test_key").unwrap(), "");
        save_runtime_value("test_key", "hello_world").unwrap();
        assert_eq!(load_runtime_value("test_key").unwrap(), "hello_world");
        save_runtime_value("test_key", "").unwrap();
        assert_eq!(load_runtime_value("test_key").unwrap(), "");
    }
}
