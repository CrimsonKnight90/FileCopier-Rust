//! Tests unitarios para el sistema de checkpoint.
//!
//! Cobertura:
//! - `mark_completed` / `mark_failed` actualizan estado correctamente
//! - `is_complete` detecta correctamente cuando no hay pendientes
//! - `save` + `load` round-trip produce estado idéntico
//! - El rename atómico garantiza que no hay archivo `.tmp` al finalizar
//! - `delete` elimina el archivo correctamente
//! - `default_path` genera path reproducible para mismo job
//! - `FlowControl`: pause/resume/cancel con `check()`

#[cfg(test)]
mod tests {    
    use std::path::PathBuf;

    use lib_core::checkpoint::{CheckpointState, FlowControl};
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
    fn mark_completed_moves_from_pending_to_completed() {
        let mut state = make_state(&["a.txt", "b.txt", "c.txt"]);
        assert_eq!(state.pending.len(),   3);
        assert_eq!(state.completed.len(), 0);

        state.mark_completed(PathBuf::from("a.txt"), Some("abc123".into()));

        assert_eq!(state.pending.len(),   2);
        assert_eq!(state.completed.len(), 1);
        assert!(!state.pending.contains(&PathBuf::from("a.txt")));
        assert!(state.completed.contains_key(&PathBuf::from("a.txt")));
    }

    #[test]
    fn mark_completed_stores_hash() {
        let mut state = make_state(&["file.bin"]);
        state.mark_completed(PathBuf::from("file.bin"), Some("deadbeef".into()));

        let stored_hash = state.completed.get(&PathBuf::from("file.bin")).unwrap();
        assert_eq!(stored_hash.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn mark_completed_without_hash_stores_none() {
        let mut state = make_state(&["file.bin"]);
        state.mark_completed(PathBuf::from("file.bin"), None);

        let stored = state.completed.get(&PathBuf::from("file.bin")).unwrap();
        assert!(stored.is_none());
    }

    #[test]
    fn mark_failed_moves_from_pending_to_failed() {
        let mut state = make_state(&["good.txt", "bad.txt"]);
        state.mark_failed(PathBuf::from("bad.txt"), "permiso denegado".into());

        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.failed.len(),  1);
        assert!(!state.pending.contains(&PathBuf::from("bad.txt")));
        assert!(state.failed.contains_key(&PathBuf::from("bad.txt")));
    }

    // ── is_complete ───────────────────────────────────────────────────────────

    #[test]
    fn is_complete_false_when_pending_exist() {
        let state = make_state(&["a.txt"]);
        assert!(!state.is_complete());
    }

    #[test]
    fn is_complete_true_when_all_processed() {
        let mut state = make_state(&["a.txt", "b.txt"]);
        state.mark_completed(PathBuf::from("a.txt"), None);
        state.mark_failed(PathBuf::from("b.txt"), "error".into());
        assert!(state.is_complete(), "Sin pendientes → is_complete() debe ser true");
    }

    #[test]
    fn empty_job_is_complete() {
        let state = make_state(&[]);
        assert!(state.is_complete(), "Job sin archivos debe ser complete");
    }

    // ── Save + Load round-trip ────────────────────────────────────────────────

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().expect("No se pudo crear directorio temporal");
        let checkpoint_path = dir.path().join("test.checkpoint");

        // Crear estado con actividad
        let mut state = make_state(&["a.txt", "b.txt", "c.txt"]);
        state.mark_completed(PathBuf::from("a.txt"), Some("hash_a".into()));
        state.mark_failed(PathBuf::from("b.txt"), "io error".into());
        // "c.txt" queda pendiente

        // Guardar
        state.save(&checkpoint_path).expect("save() no debe fallar");
        assert!(checkpoint_path.exists(), "El archivo de checkpoint debe existir");

        // Cargar
        let loaded = CheckpointState::load(&checkpoint_path).expect("load() no debe fallar");

        // Verificar que el estado es idéntico
        assert_eq!(loaded.job_id,         state.job_id);
        assert_eq!(loaded.completed.len(), 1);
        assert_eq!(loaded.failed.len(),    1);
        assert_eq!(loaded.pending.len(),   1);
        assert!(loaded.pending.contains(&PathBuf::from("c.txt")));

        let hash = loaded.completed.get(&PathBuf::from("a.txt")).unwrap();
        assert_eq!(hash.as_deref(), Some("hash_a"));
    }

    #[test]
    fn save_no_tmp_file_after_success() {
        let dir = tempfile::tempdir().unwrap();
        let checkpoint_path = dir.path().join("test.checkpoint");
        let tmp_path = checkpoint_path.with_extension("tmp");

        let state = make_state(&["x.dat"]);
        state.save(&checkpoint_path).unwrap();

        assert!( checkpoint_path.exists(), "Checkpoint debe existir");
        assert!(!tmp_path.exists(),        "Archivo .tmp no debe quedar tras save exitoso");
    }

    #[test]
    fn load_nonexistent_returns_error() {
        let result = CheckpointState::load(std::path::Path::new("/no/existe/checkpoint"));
        assert!(result.is_err());
    }

    // ── delete ────────────────────────────────────────────────────────────────

    #[test]
    fn delete_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let cp_path = dir.path().join("del.checkpoint");

        let state = make_state(&[]);
        state.save(&cp_path).unwrap();
        assert!(cp_path.exists());

        CheckpointState::delete(&cp_path).expect("delete() no debe fallar");
        assert!(!cp_path.exists(), "Checkpoint debe haber sido eliminado");
    }

    #[test]
    fn delete_nonexistent_is_ok() {
        // Eliminar un checkpoint que no existe no debe ser error
        let result = CheckpointState::delete(std::path::Path::new("/no/existe"));
        assert!(result.is_ok());
    }

    // ── default_path ──────────────────────────────────────────────────────────

    #[test]
    fn default_path_is_reproducible() {
        let dest = PathBuf::from("/destino");
        let p1 = CheckpointState::default_path(&dest, "job-abc");
        let p2 = CheckpointState::default_path(&dest, "job-abc");
        assert_eq!(p1, p2, "El mismo job debe producir el mismo path de checkpoint");
    }

    #[test]
    fn default_path_different_jobs_are_different() {
        let dest = PathBuf::from("/destino");
        let p1 = CheckpointState::default_path(&dest, "job-001");
        let p2 = CheckpointState::default_path(&dest, "job-002");
        assert_ne!(p1, p2);
    }

    #[test]
    fn default_path_is_inside_dest() {
        let dest = PathBuf::from("/mi/destino");
        let cp   = CheckpointState::default_path(&dest, "job-x");
        assert!(
            cp.starts_with(&dest),
            "El checkpoint debe estar dentro del directorio destino"
        );
    }

    // ── FlowControl ───────────────────────────────────────────────────────────

    #[test]
    fn flow_control_starts_unpaused_uncancelled() {
        let fc = FlowControl::new();
        assert!(!fc.is_paused());
        assert!(!fc.is_cancelled());
        assert!(fc.check().is_ok());
    }

    #[test]
    fn flow_control_pause_makes_check_return_paused() {
        let fc = FlowControl::new();
        fc.pause();
        assert!(fc.is_paused());

        match fc.check() {
            Err(CoreError::Paused) => {},
            other => panic!("Esperaba CoreError::Paused, obtuvo: {other:?}"),
        }
    }

    #[test]
    fn flow_control_resume_after_pause() {
        let fc = FlowControl::new();
        fc.pause();
        assert!(fc.is_paused());
        fc.resume();
        assert!(!fc.is_paused());
        assert!(fc.check().is_ok());
    }

    #[test]
    fn flow_control_cancel_makes_check_return_disconnected() {
        let fc = FlowControl::new();
        fc.cancel();
        assert!(fc.is_cancelled());

        match fc.check() {
            Err(CoreError::PipelineDisconnected) => {},
            other => panic!("Esperaba PipelineDisconnected, obtuvo: {other:?}"),
        }
    }

    #[test]
    fn flow_control_cancel_overrides_pause() {
        let fc = FlowControl::new();
        fc.pause();
        fc.cancel();

        // Cancel tiene prioridad: check retorna PipelineDisconnected, no Paused
        match fc.check() {
            Err(CoreError::PipelineDisconnected) => {},
            other => panic!("Cancel debe tener prioridad sobre pause: {other:?}"),
        }
    }

    #[test]
    fn flow_control_clone_shares_state() {
        let fc1 = FlowControl::new();
        let fc2 = fc1.clone();

        fc1.pause();
        assert!(fc2.is_paused(), "Clone debe compartir el mismo estado atómico");

        fc2.resume();
        assert!(!fc1.is_paused(), "Cambios en el clone deben reflejarse en el original");
    }
}