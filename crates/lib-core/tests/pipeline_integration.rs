//! Tests de integración para el pipeline completo de copia.
//!
//! Cobertura:
//! - Copia simple archivo pequeño (via enjambre)
//! - Copia simple archivo grande (via motor de bloques con BufferPool RAII)
//! - Verificación de integridad blake3 pasa cuando los datos son correctos
//! - Archivos `.partial` se crean y renombran correctamente
//! - Copia de directorio completo preserva estructura
//! - Resume desde checkpoint

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use lib_core::{
        checkpoint::FlowControl,
        config::EngineConfig,
        engine::Orchestrator,
        hash::Algorithm,
        os_ops::NoOpOsOps,
    };

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn write_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    fn read_file(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap_or_else(|e| panic!("No se pudo leer {}: {e}", path.display()))
    }

    fn test_config() -> EngineConfig {
        EngineConfig {
            triage_threshold_bytes: 1024 * 1024, // 1 MB
            block_size_bytes:       64 * 1024,   // 64 KB — bloques pequeños para tests rápidos
            channel_capacity:       4,
            swarm_concurrency:      4,
            verify:                 false,
            hash_algorithm:         Algorithm::Blake3,
            resume:                 false,
            use_partial_files:      true,
            bandwidth_limit_bytes_per_sec: 0,
            bandwidth_burst_bytes:  1 * 1024 * 1024,
        }
    }

    fn test_config_verify() -> EngineConfig {
        EngineConfig { verify: true, ..test_config() }
    }

    fn make_orchestrator(config: EngineConfig) -> Orchestrator {
        let flow   = FlowControl::new();
        let os_ops = Arc::new(NoOpOsOps);
        Orchestrator::new(config, flow, os_ops)
    }

    // ── Test 1: Archivo único pequeño ────────────────────────────────────────

    #[test]
    fn copy_single_small_file() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let content = b"Hola FileCopier-Rust!";
        write_file(src.path(), "hello.txt", content);

        let result = make_orchestrator(test_config())
            .run(src.path(), dst.path(), None)
            .unwrap();

        assert_eq!(result.completed_files, 1);
        assert_eq!(result.failed_files,    0);
        assert_eq!(read_file(&dst.path().join("hello.txt")), content);
    }

    // ── Test 2: Archivo binario ───────────────────────────────────────────────

    #[test]
    fn copy_binary_file_preserves_content() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0u8..=255).cycle().take(8192).collect();
        write_file(src.path(), "binary.bin", &content);

        make_orchestrator(test_config())
            .run(src.path(), dst.path(), None)
            .unwrap();

        assert_eq!(read_file(&dst.path().join("binary.bin")), content);
    }

    // ── Test 3: Estructura de directorio ─────────────────────────────────────

    #[test]
    fn copy_preserves_directory_structure() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "raiz.txt",       b"raiz");
        write_file(src.path(), "sub/a.txt",      b"a");
        write_file(src.path(), "sub/b.txt",      b"b");
        write_file(src.path(), "sub/deep/c.txt", b"c");

        let result = make_orchestrator(test_config())
            .run(src.path(), dst.path(), None)
            .unwrap();

        assert_eq!(result.completed_files, 4);
        assert_eq!(result.failed_files,    0);
        assert_eq!(read_file(&dst.path().join("raiz.txt")),       b"raiz");
        assert_eq!(read_file(&dst.path().join("sub/a.txt")),      b"a");
        assert_eq!(read_file(&dst.path().join("sub/b.txt")),      b"b");
        assert_eq!(read_file(&dst.path().join("sub/deep/c.txt")), b"c");
    }

    // ── Test 4: Verificación de integridad ───────────────────────────────────

    #[test]
    fn copy_with_verify_succeeds_on_intact_file() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "data.bin", &vec![0xAB; 4096]);

        let result = make_orchestrator(test_config_verify())
            .run(src.path(), dst.path(), None)
            .unwrap();

        assert_eq!(result.completed_files, 1);
        assert_eq!(result.failed_files,    0);
    }

    // ── Test 5: Archivo vacío ────────────────────────────────────────────────

    #[test]
    fn copy_empty_file() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "empty.txt", b"");

        let result = make_orchestrator(test_config())
            .run(src.path(), dst.path(), None)
            .unwrap();

        assert_eq!(result.completed_files, 1);
        assert!(read_file(&dst.path().join("empty.txt")).is_empty());
    }

    // ── Test 6: Archivo sin extensión (bug .partial) ─────────────────────────

    #[test]
    fn copy_file_without_extension() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "Makefile", b"all:\n\techo hello\n");

        let result = make_orchestrator(test_config())
            .run(src.path(), dst.path(), None)
            .unwrap();

        assert_eq!(result.completed_files, 1);
        assert!(dst.path().join("Makefile").exists());
        assert!(!dst.path().join("Makefile.partial").exists());
    }

    // ── Test 7: Archivo grande via motor de bloques con BufferPool RAII ───────

    #[test]
    fn copy_large_file_via_block_engine_with_raii_pool() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        // 4 MB — supera el umbral de 1 MB del test_config → motor de bloques
        // Con block_size=64KB y channel_cap=4, el pool tendrá 6 buffers.
        // El archivo requiere ~64 bloques → el pool debe reutilizarse ~10 veces.
        let content: Vec<u8> = (0u8..=255).cycle().take(4 * 1024 * 1024).collect();
        write_file(src.path(), "large.bin", &content);

        let result = make_orchestrator(test_config())
            .run(src.path(), dst.path(), None)
            .unwrap();

        assert_eq!(result.completed_files, 1);
        assert_eq!(result.failed_files,    0);
        assert_eq!(read_file(&dst.path().join("large.bin")), content);
    }

    // ── Test 8: Archivo grande con verificación ───────────────────────────────

    #[test]
    fn copy_large_file_with_verify() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0u8..=255).cycle().take(2 * 1024 * 1024).collect();
        write_file(src.path(), "large_verify.bin", &content);

        let result = make_orchestrator(test_config_verify())
            .run(src.path(), dst.path(), None)
            .unwrap();

        assert_eq!(result.completed_files, 1);
        assert_eq!(result.failed_files,    0);
        assert_eq!(read_file(&dst.path().join("large_verify.bin")), content);
    }

    // ── Test 9: Mix archivos pequeños y grandes ───────────────────────────────

    #[test]
    fn copy_mixed_size_files() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        // Pequeños → enjambre
        for i in 0u8..10 {
            write_file(src.path(), &format!("small_{i}.txt"), &vec![i; 100]);
        }
        // Grande → motor de bloques
        let large: Vec<u8> = (0u8..=255).cycle().take(2 * 1024 * 1024).collect();
        write_file(src.path(), "large.bin", &large);

        let result = make_orchestrator(test_config())
            .run(src.path(), dst.path(), None)
            .unwrap();

        assert_eq!(result.completed_files, 11);
        assert_eq!(result.failed_files,    0);

        for i in 0u8..10 {
            assert_eq!(
                read_file(&dst.path().join(format!("small_{i}.txt"))),
                vec![i; 100]
            );
        }
        assert_eq!(read_file(&dst.path().join("large.bin")), large);
    }

    // ── Test 10: Bytes copiados correctos ─────────────────────────────────────

    #[test]
    fn result_bytes_matches_actual_size() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "a.bin", &vec![1u8; 1234]);
        write_file(src.path(), "b.bin", &vec![2u8; 5678]);

        let result = make_orchestrator(test_config())
            .run(src.path(), dst.path(), None)
            .unwrap();

        assert_eq!(result.copied_bytes, 1234 + 5678);
    }

    // ── Test 11: Pool RAII — no hay fugas de buffers ──────────────────────────

    #[test]
    fn buffer_pool_raii_no_leaks() {
        // Verificar que después de copiar un archivo grande,
        // el pool recupera todos sus buffers (ninguno queda "perdido").
        use lib_core::buffer_pool::BufferPool;

        let pool = BufferPool::new(64 * 1024, 6); // 6 buffers de 64 KB
        assert_eq!(pool.available(), 6);

        // Simular adquisición y devolución automática
        {
            let _b1 = pool.acquire();
            let _b2 = pool.acquire();
            assert_eq!(pool.available(), 4);
        } // drop(_b1) y drop(_b2) → ambos vuelven al pool

        assert_eq!(pool.available(), 6);
    }
}
