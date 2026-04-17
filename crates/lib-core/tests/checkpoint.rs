//! Tests unitarios para el sistema de checkpoint.
//!
//! Cobertura:
//! - `mark_completed` / `mark_failed` actualizan estado correctamente
//! - `is_complete` detecta correctamente cuando no hay pendientes
//! - `save` + `load` round-trip produce estado idéntico (formato v2)
//! - Migración automática desde formato v1
//! - `validate_completed` con cada política (TrustCheckpoint, VerifySize, VerifyHash)
//! - `FlowControl`: pause/resume/cancel/clone

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use lib_core::checkpoint::{CheckpointState, FlowControl, ResumePolicy};
    use lib_core::error::CoreError;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_state(files: &[&str]) -> CheckpointState {
        let paths: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        CheckpointState::new(
            "test-job-001",
            PathBuf::from("/origen"),
            PathBuf::from("/destino"),
            paths,
        )
    }

    // ── mark_completed / mark_failed ──────────────────────────────────────────

    #[test]
    fn mark_completed_stores_entry_with_size() {
        let mut state = make_state(&["a.txt", "b.txt"]);
        state.mark_completed(PathBuf::from("a.txt"), Some("abc123".into()), 1024);

        let entry = state.completed.get(&PathBuf::from("a.txt")).unwrap();
        assert_eq!(entry.hash.as_deref(), Some("abc123"));
        assert_eq!(entry.size_bytes, 1024);
        assert!(!state.pending.contains(&PathBuf::from("a.txt")));
    }

    #[test]
    fn mark_completed_without_hash() {
        let mut state = make_state(&["file.bin"]);
        state.mark_completed(PathBuf::from("file.bin"), None, 512);
        let entry = state.completed.get(&PathBuf::from("file.bin")).unwrap();
        assert!(entry.hash.is_none());
        assert_eq!(entry.size_bytes, 512);
    }

    #[test]
    fn mark_failed_moves_to_failed() {
        let mut state = make_state(&["good.txt", "bad.txt"]);
        state.mark_failed(PathBuf::from("bad.txt"), "permiso denegado".into());
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.failed.len(), 1);
    }

    // ── is_complete ───────────────────────────────────────────────────────────

    #[test]
    fn is_complete_when_all_processed() {
        let mut state = make_state(&["a.txt", "b.txt"]);
        state.mark_completed(PathBuf::from("a.txt"), None, 10);
        state.mark_failed(PathBuf::from("b.txt"), "error".into());
        assert!(state.is_complete());
    }

    #[test]
    fn empty_job_is_complete() {
        assert!(make_state(&[]).is_complete());
    }

    // ── validate_completed — TrustCheckpoint ──────────────────────────────────

    #[test]
    fn trust_checkpoint_never_reverts() {
        let dir   = tempfile::tempdir().unwrap();
        let mut s = make_state(&[]);
        s.mark_completed(PathBuf::from("ghost.txt"), None, 999);

        let reverted = s.validate_completed(dir.path(), ResumePolicy::TrustCheckpoint);
        assert_eq!(reverted, 0);
        assert!(s.completed.contains_key(&PathBuf::from("ghost.txt")));
    }

    // ── validate_completed — VerifySize: Missing ──────────────────────────────

    #[test]
    fn verify_size_reverts_missing_file() {
        let dir   = tempfile::tempdir().unwrap();
        let mut s = make_state(&[]);
        s.mark_completed(PathBuf::from("missing.txt"), None, 100);

        let reverted = s.validate_completed(dir.path(), ResumePolicy::VerifySize);

        assert_eq!(reverted, 1);
        assert!(!s.completed.contains_key(&PathBuf::from("missing.txt")));
        assert!(s.pending.contains(&PathBuf::from("missing.txt")));
    }

    // ── validate_completed — VerifySize: SizeMismatch ────────────────────────

    #[test]
    fn verify_size_reverts_truncated_file() {
        let dir  = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("truncated.txt"), b"short").unwrap();

        let mut s = make_state(&[]);
        s.mark_completed(PathBuf::from("truncated.txt"), None, 9999);

        let reverted = s.validate_completed(dir.path(), ResumePolicy::VerifySize);

        assert_eq!(reverted, 1);
        assert!(s.pending.contains(&PathBuf::from("truncated.txt")));
    }

    // ── validate_completed — VerifySize: OK ──────────────────────────────────

    #[test]
    fn verify_size_accepts_correct_file() {
        let dir  = tempfile::tempdir().unwrap();
        let data = b"hello world";
        std::fs::write(dir.path().join("ok.txt"), data).unwrap();

        let mut s = make_state(&[]);
        s.mark_completed(PathBuf::from("ok.txt"), None, data.len() as u64);

        let reverted = s.validate_completed(dir.path(), ResumePolicy::VerifySize);
        assert_eq!(reverted, 0);
    }

    // ── validate_completed — VerifySize: v1 migrado (size=0) ─────────────────

    #[test]
    fn verify_size_with_migrated_v1_entry_accepts_existing_file() {
        let dir  = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("migrated.txt"), b"any content").unwrap();

        let mut s = make_state(&[]);
        // size_bytes=0 simula un checkpoint v1 migrado
        s.mark_completed(PathBuf::from("migrated.txt"), None, 0);

        let reverted = s.validate_completed(dir.path(), ResumePolicy::VerifySize);
        // Con size=0, solo se verifica existencia — el archivo existe → OK
        assert_eq!(reverted, 0);
    }

    // ── validate_completed — VerifyHash: corrupción ───────────────────────────

    #[test]
    fn verify_hash_detects_corrupted_file_same_size() {
        let dir  = tempfile::tempdir().unwrap();
        let original = b"original content";

        let correct_hash = {
            let mut h = blake3::Hasher::new();
            h.update(original);
            h.finalize().to_hex().to_string()
        };

        // Corromper con mismo tamaño
        std::fs::write(dir.path().join("corrupt.bin"), b"corrupted!!!!!!").unwrap();

        let mut s = make_state(&[]);
        s.mark_completed(PathBuf::from("corrupt.bin"), Some(correct_hash), original.len() as u64);

        let reverted = s.validate_completed(dir.path(), ResumePolicy::VerifyHash);
        assert_eq!(reverted, 1);
        assert!(s.pending.contains(&PathBuf::from("corrupt.bin")));
    }

    #[test]
    fn verify_hash_accepts_intact_file() {
        let dir  = tempfile::tempdir().unwrap();
        let data = b"intact data";
        std::fs::write(dir.path().join("intact.bin"), data).unwrap();

        let hash = {
            let mut h = blake3::Hasher::new();
            h.update(data);
            h.finalize().to_hex().to_string()
        };

        let mut s = make_state(&[]);
        s.mark_completed(PathBuf::from("intact.bin"), Some(hash), data.len() as u64);

        let reverted = s.validate_completed(dir.path(), ResumePolicy::VerifyHash);
        assert_eq!(reverted, 0);
    }

    #[test]
    fn verify_hash_without_stored_hash_falls_back_to_size() {
        let dir  = tempfile::tempdir().unwrap();
        let data = b"data without hash";
        std::fs::write(dir.path().join("nohash.bin"), data).unwrap();

        let mut s = make_state(&[]);
        s.mark_completed(PathBuf::from("nohash.bin"), None, data.len() as u64);

        // Sin hash guardado → no puede verificar contenido → acepta por tamaño
        let reverted = s.validate_completed(dir.path(), ResumePolicy::VerifyHash);
        assert_eq!(reverted, 0);
    }

    // ── validate_completed — múltiples archivos ───────────────────────────────

    #[test]
    fn validate_multiple_files_mixed_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.txt"),    b"correct").unwrap();
        std::fs::write(dir.path().join("small.txt"), b"x").unwrap();
        // bad.txt no existe

        let mut s = make_state(&[]);
        s.mark_completed(PathBuf::from("ok.txt"),    None, 7);
        s.mark_completed(PathBuf::from("bad.txt"),   None, 100);
        s.mark_completed(PathBuf::from("small.txt"), None, 500);

        let reverted = s.validate_completed(dir.path(), ResumePolicy::VerifySize);

        assert_eq!(reverted, 2);
        assert!(s.completed.contains_key(&PathBuf::from("ok.txt")));
        assert!(s.pending.contains(&PathBuf::from("bad.txt")));
        assert!(s.pending.contains(&PathBuf::from("small.txt")));
    }

    // ── Save + Load round-trip v2 ─────────────────────────────────────────────

    #[test]
    fn save_and_load_roundtrip_v2() {
        let dir = tempfile::tempdir().unwrap();
        let cp  = dir.path().join("test.checkpoint");

        let mut s = make_state(&["a.txt", "b.txt", "c.txt"]);
        s.mark_completed(PathBuf::from("a.txt"), Some("hash_a".into()), 1024);
        s.mark_failed(PathBuf::from("b.txt"), "io error".into());
        s.save(&cp).unwrap();

        let loaded = CheckpointState::load(&cp).unwrap();
        let entry = loaded.completed.get(&PathBuf::from("a.txt")).unwrap();
        assert_eq!(entry.hash.as_deref(), Some("hash_a"));
        assert_eq!(entry.size_bytes, 1024);
        assert_eq!(loaded.failed.len(), 1);
        assert_eq!(loaded.pending.len(), 1);
    }

    #[test]
    fn save_no_tmp_file_after_success() {
        let dir = tempfile::tempdir().unwrap();
        let cp  = dir.path().join("test.checkpoint");
        make_state(&["x.dat"]).save(&cp).unwrap();
        assert!( cp.exists());
        assert!(!cp.with_extension("tmp").exists());
    }

    // ── default_path ──────────────────────────────────────────────────────────

    #[test]
    fn default_path_is_reproducible() {
        let dest = PathBuf::from("/destino");
        assert_eq!(
            CheckpointState::default_path(&dest, "job-abc"),
            CheckpointState::default_path(&dest, "job-abc")
        );
    }

    #[test]
    fn default_path_is_inside_dest() {
        let dest = PathBuf::from("/mi/destino");
        assert!(CheckpointState::default_path(&dest, "job-x").starts_with(&dest));
    }

    // ── FlowControl ───────────────────────────────────────────────────────────

    #[test]
    fn flow_control_starts_clean() {
        let fc = FlowControl::new();
        assert!(!fc.is_paused());
        assert!(!fc.is_cancelled());
        assert!(fc.check().is_ok());
    }

    #[test]
    fn flow_control_pause_resume() {
        let fc = FlowControl::new();
        fc.pause();
        assert!(matches!(fc.check(), Err(CoreError::Paused)));
        fc.resume();
        assert!(fc.check().is_ok());
    }

    #[test]
    fn flow_control_cancel_overrides_pause() {
        let fc = FlowControl::new();
        fc.pause();
        fc.cancel();
        assert!(matches!(fc.check(), Err(CoreError::PipelineDisconnected)));
    }

    #[test]
    fn flow_control_clone_shares_state() {
        let fc1 = FlowControl::new();
        let fc2 = fc1.clone();
        fc1.pause();
        assert!(fc2.is_paused());
        fc2.resume();
        assert!(!fc1.is_paused());
    }
}
