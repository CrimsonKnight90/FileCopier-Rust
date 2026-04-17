//! Tests de integración para los modos dry-run y move.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use lib_core::{
        checkpoint::FlowControl,
        config::{EngineConfig, OperationMode},
        engine::Orchestrator,
        os_ops::NoOpOsOps,
    };

    fn make_orch(config: EngineConfig) -> Orchestrator {
        Orchestrator::new(config, FlowControl::new(), Arc::new(NoOpOsOps))
    }

    fn small_config() -> EngineConfig {
        EngineConfig {
            triage_threshold_bytes: 1024 * 1024,
            block_size_bytes:       64 * 1024,
            channel_capacity:       4,
            swarm_concurrency:      4,
            ..EngineConfig::default()
        }
    }

    // ── Dry-run: no escribe nada ──────────────────────────────────────────────

    #[test]
    fn dry_run_does_not_write_files() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        fs::write(src.path().join("file.txt"), b"hello").unwrap();
        fs::write(src.path().join("other.bin"), b"world world").unwrap();

        let config = EngineConfig {
            dry_run: true,
            ..small_config()
        };

        let result = make_orch(config).run(src.path(), dst.path(), None).unwrap();

        // Nada debe haberse creado en destino
        assert!(!dst.path().join("file.txt").exists());
        assert!(!dst.path().join("other.bin").exists());
        // El reporte debe existir
        assert!(result.dry_run_report.is_some());
    }

    #[test]
    fn dry_run_counts_files_and_bytes() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        fs::write(src.path().join("a.txt"), b"1234567890").unwrap(); // 10 bytes
        fs::write(src.path().join("b.txt"), b"12345").unwrap();       //  5 bytes

        let config = EngineConfig { dry_run: true, ..small_config() };
        let result = make_orch(config).run(src.path(), dst.path(), None).unwrap();

        let report = result.dry_run_report.unwrap();
        assert_eq!(report.total_files, 2);
        assert_eq!(report.total_bytes, 15);
        assert!(report.is_safe);
    }

    #[test]
    fn dry_run_move_shows_move_actions() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        fs::write(src.path().join("data.bin"), b"move me").unwrap();

        let config = EngineConfig {
            dry_run:        true,
            operation_mode: OperationMode::Move,
            ..small_config()
        };

        let result = make_orch(config).run(src.path(), dst.path(), None).unwrap();

        let report = result.dry_run_report.unwrap();
        assert_eq!(report.total_files, 1);

        // Verificar que la acción es Move
        use lib_core::engine::dry_run::PlannedAction;
        assert!(matches!(&report.actions[0], PlannedAction::Move { .. }));

        // El origen no debe haber sido tocado (dry-run)
        assert!(src.path().join("data.bin").exists());
    }

    #[test]
    fn dry_run_detects_already_existing_file() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let data = b"same content";

        fs::write(src.path().join("existing.txt"), data).unwrap();
        fs::write(dst.path().join("existing.txt"), data).unwrap(); // mismo tamaño

        let config = EngineConfig { dry_run: true, ..small_config() };
        let result = make_orch(config).run(src.path(), dst.path(), None).unwrap();

        let report = result.dry_run_report.unwrap();
        assert_eq!(report.total_files,   0); // no se copiaría nada
        assert_eq!(report.skipped_files, 1); // se saltaría
    }

    // ── Move: origen se borra tras copia exitosa ──────────────────────────────

    #[test]
    fn move_deletes_source_after_successful_copy() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let data = b"content to move";

        fs::write(src.path().join("moveme.txt"), data).unwrap();

        let config = EngineConfig {
            operation_mode: OperationMode::Move,
            ..small_config()
        };

        let result = make_orch(config).run(src.path(), dst.path(), None).unwrap();

        // Destino existe con el contenido correcto
        assert!(dst.path().join("moveme.txt").exists());
        assert_eq!(fs::read(dst.path().join("moveme.txt")).unwrap(), data);

        // Origen fue borrado
        assert!(!src.path().join("moveme.txt").exists());

        assert_eq!(result.failed_files, 0);
    }

    #[test]
    fn move_preserves_directory_structure() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        fs::create_dir(src.path().join("sub")).unwrap();
        fs::write(src.path().join("root.txt"),    b"root").unwrap();
        fs::write(src.path().join("sub/deep.txt"), b"deep").unwrap();

        let config = EngineConfig {
            operation_mode: OperationMode::Move,
            ..small_config()
        };

        let result = make_orch(config).run(src.path(), dst.path(), None).unwrap();

        assert_eq!(result.completed_files, 2);
        assert_eq!(result.failed_files,    0);
        assert!(dst.path().join("root.txt").exists());
        assert!(dst.path().join("sub/deep.txt").exists());
        assert!(!src.path().join("root.txt").exists());
        assert!(!src.path().join("sub/deep.txt").exists());
    }

    #[test]
    fn move_with_verify_checks_integrity_before_delete() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0u8..=255).cycle().take(512).collect();

        fs::write(src.path().join("verified.bin"), &data).unwrap();

        let config = EngineConfig {
            operation_mode: OperationMode::Move,
            verify:         true,
            ..small_config()
        };

        let result = make_orch(config).run(src.path(), dst.path(), None).unwrap();

        assert_eq!(result.failed_files, 0);
        // Destino correcto
        assert_eq!(fs::read(dst.path().join("verified.bin")).unwrap(), data);
        // Origen borrado
        assert!(!src.path().join("verified.bin").exists());
    }

    // ── Move copy-only: origen debe quedar intacto si la copia falla ──────────

    #[test]
    fn copy_mode_never_deletes_source() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let data = b"keep me";

        fs::write(src.path().join("keep.txt"), data).unwrap();

        let config = small_config(); // OperationMode::Copy por default

        let result = make_orch(config).run(src.path(), dst.path(), None).unwrap();

        // Origen intacto
        assert!(src.path().join("keep.txt").exists());
        // Destino creado
        assert!(dst.path().join("keep.txt").exists());
        assert_eq!(result.failed_files, 0);
    }

    // ── Dry-run es idempotente ─────────────────────────────────────────────────

    #[test]
    fn dry_run_is_idempotent() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        fs::write(src.path().join("data.txt"), b"idempotent").unwrap();

        let config = EngineConfig { dry_run: true, ..small_config() };

        // Ejecutar dos veces — debe producir el mismo resultado
        let r1 = make_orch(config.clone()).run(src.path(), dst.path(), None).unwrap();
        let r2 = make_orch(config).run(src.path(), dst.path(), None).unwrap();

        let rep1 = r1.dry_run_report.unwrap();
        let rep2 = r2.dry_run_report.unwrap();

        assert_eq!(rep1.total_files,  rep2.total_files);
        assert_eq!(rep1.total_bytes,  rep2.total_bytes);
        assert_eq!(rep1.is_safe,      rep2.is_safe);

        // Destino sigue vacío
        assert!(!dst.path().join("data.txt").exists());
    }
}
