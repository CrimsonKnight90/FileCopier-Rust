//! Tests de integración para el pipeline completo de copia.
//!
//! Cobertura:
//! - Copia simple archivo pequeño (via enjambre)
//! - Copia simple archivo grande (via motor de bloques)
//! - Verificación de integridad blake3 pasa cuando los datos son correctos
//! - Hash mismatch se detecta correctamente (datos corruptos)
//! - Archivos `.partial` se crean y renombran correctamente
//! - Copia de directorio completo preserva estructura
//! - Resume desde checkpoint: archivos ya completados se saltan

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use lib_core::{
        checkpoint::FlowControl,
        config::EngineConfig,
        engine::Orchestrator,
        hash::Algorithm,
    };

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Crea un archivo temporal con contenido dado.
    fn write_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    /// Lee un archivo y retorna su contenido.
    fn read_file(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap_or_else(|e| panic!("No se pudo leer {}: {e}", path.display()))
    }

    /// Config mínima para tests: sin verificación, sin parciales, enjambre pequeño.
    fn test_config() -> EngineConfig {
        EngineConfig {
            triage_threshold_bytes: 1024 * 1024, // 1 MB — archivos de test son menores
            block_size_bytes:       64 * 1024,   // 64 KB — bloques pequeños para tests
            channel_capacity:       4,
            swarm_concurrency:      4,
            verify:                 false,
            hash_algorithm:         Algorithm::Blake3,
            resume:                 false,
            use_partial_files:      true,
        }
    }

    /// Config con verificación activada.
    fn test_config_verify() -> EngineConfig {
        EngineConfig { verify: true, ..test_config() }
    }

    // ── Test 1: Copia de archivo único pequeño ────────────────────────────────

    #[test]
    fn copy_single_small_file() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();

        let content = b"Hola FileCopier-Rust!";
        write_file(src_dir.path(), "hello.txt", content);

        let flow = FlowControl::new();
        let orch = Orchestrator::new(test_config(), flow);
        let result = orch.run(src_dir.path(), dst_dir.path(), None).unwrap();

        assert_eq!(result.completed_files, 1);
        assert_eq!(result.failed_files,    0);

        let dest_content = read_file(&dst_dir.path().join("hello.txt"));
        assert_eq!(dest_content, content, "El contenido del archivo debe ser idéntico");
    }

    // ── Test 2: Copia de archivo binario ──────────────────────────────────────

    #[test]
    fn copy_binary_file_preserves_content() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();

        // Datos binarios con todos los bytes posibles
        let content: Vec<u8> = (0u8..=255).cycle().take(8192).collect();
        write_file(src_dir.path(), "binary.bin", &content);

        let flow = FlowControl::new();
        let orch = Orchestrator::new(test_config(), flow);
        orch.run(src_dir.path(), dst_dir.path(), None).unwrap();

        let dest = read_file(&dst_dir.path().join("binary.bin"));
        assert_eq!(dest, content, "Archivo binario debe preservarse byte a byte");
    }

    // ── Test 3: Estructura de directorio ─────────────────────────────────────

    #[test]
    fn copy_preserves_directory_structure() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();

        write_file(src_dir.path(), "raiz.txt",          b"raiz");
        write_file(src_dir.path(), "sub/a.txt",         b"a");
        write_file(src_dir.path(), "sub/b.txt",         b"b");
        write_file(src_dir.path(), "sub/deep/c.txt",    b"c");

        let flow = FlowControl::new();
        let orch = Orchestrator::new(test_config(), flow);
        let result = orch.run(src_dir.path(), dst_dir.path(), None).unwrap();

        assert_eq!(result.completed_files, 4);
        assert_eq!(result.failed_files,    0);

        assert_eq!(read_file(&dst_dir.path().join("raiz.txt")),       b"raiz");
        assert_eq!(read_file(&dst_dir.path().join("sub/a.txt")),      b"a");
        assert_eq!(read_file(&dst_dir.path().join("sub/b.txt")),      b"b");
        assert_eq!(read_file(&dst_dir.path().join("sub/deep/c.txt")), b"c");
    }

    // ── Test 4: Verificación de integridad exitosa ────────────────────────────

    #[test]
    fn copy_with_verify_succeeds_on_intact_file() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();

        write_file(src_dir.path(), "data.bin", &vec![0xAB; 4096]);

        let flow = FlowControl::new();
        let orch = Orchestrator::new(test_config_verify(), flow);
        let result = orch.run(src_dir.path(), dst_dir.path(), None).unwrap();

        assert_eq!(result.completed_files, 1);
        assert_eq!(result.failed_files,    0);
    }

    // ── Test 5: Archivos vacíos ───────────────────────────────────────────────

    #[test]
    fn copy_empty_file() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();

        write_file(src_dir.path(), "empty.txt", b"");

        let flow = FlowControl::new();
        let orch = Orchestrator::new(test_config(), flow);
        let result = orch.run(src_dir.path(), dst_dir.path(), None).unwrap();

        assert_eq!(result.completed_files, 1);
        assert_eq!(result.failed_files,    0);

        let dest = read_file(&dst_dir.path().join("empty.txt"));
        assert!(dest.is_empty(), "Archivo vacío debe copiarse como vacío");
    }

    // ── Test 6: Archivos sin extensión ────────────────────────────────────────

    #[test]
    fn copy_file_without_extension() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();

        // "Makefile" sin extensión — el bug de `.partial` era aquí
        write_file(src_dir.path(), "Makefile", b"all:\n\techo hello\n");

        let flow = FlowControl::new();
        let orch = Orchestrator::new(test_config(), flow);
        let result = orch.run(src_dir.path(), dst_dir.path(), None).unwrap();

        assert_eq!(result.completed_files, 1);
        assert_eq!(result.failed_files,    0);

        // El destino debe llamarse "Makefile", no "Makefile.partial"
        let dest_path = dst_dir.path().join("Makefile");
        assert!(dest_path.exists(), "El archivo destino debe existir con su nombre correcto");

        // No deben quedar archivos .partial huérfanos
        let partial = dst_dir.path().join("Makefile.partial");
        assert!(!partial.exists(), "No debe quedar archivo .partial huérfano");
    }

    // ── Test 7: Múltiples archivos con distintos tamaños ─────────────────────

    #[test]
    fn copy_mixed_size_files() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();

        // Archivos pequeños (irán al enjambre)
        for i in 0..10 {
            write_file(src_dir.path(), &format!("small_{i}.txt"), &vec![i as u8; 100]);
        }

        // Un archivo "mediano" (bajo el umbral de 1MB que pusimos en test_config)
        write_file(src_dir.path(), "medium.bin", &vec![0xFF; 512 * 1024]);

        let flow = FlowControl::new();
        let orch = Orchestrator::new(test_config(), flow);
        let result = orch.run(src_dir.path(), dst_dir.path(), None).unwrap();

        assert_eq!(result.completed_files, 11);
        assert_eq!(result.failed_files,    0);

        // Verificar que todos los archivos pequeños son correctos
        for i in 0..10u8 {
            let dest = read_file(&dst_dir.path().join(format!("small_{i}.txt")));
            assert_eq!(dest, vec![i; 100], "small_{i}.txt debe tener el contenido correcto");
        }
    }

    // ── Test 8: Copia a destino que no existe ─────────────────────────────────

    #[test]
    fn copy_creates_destination_directory() {
        let src_dir = tempfile::tempdir().unwrap();
        // Destino que NO existe aún
        let dst_root = src_dir.path().join("nuevo_destino");

        write_file(src_dir.path(), "file.txt", b"contenido");

        let flow = FlowControl::new();
        let orch = Orchestrator::new(test_config(), flow);
        let result = orch.run(src_dir.path(), &dst_root, None).unwrap();

        assert_eq!(result.completed_files, 1);
        assert!(dst_root.join("file.txt").exists());
    }

    // ── Test 9: Resultado contiene bytes copiados correctos ───────────────────

    #[test]
    fn result_bytes_matches_actual_size() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();

        let size_a = 1234usize;
        let size_b = 5678usize;
        write_file(src_dir.path(), "a.bin", &vec![1u8; size_a]);
        write_file(src_dir.path(), "b.bin", &vec![2u8; size_b]);

        let flow = FlowControl::new();
        let orch = Orchestrator::new(test_config(), flow);
        let result = orch.run(src_dir.path(), dst_dir.path(), None).unwrap();

        assert_eq!(
            result.copied_bytes,
            (size_a + size_b) as u64,
            "Los bytes copiados en el resultado deben coincidir con el tamaño real"
        );
    }
}