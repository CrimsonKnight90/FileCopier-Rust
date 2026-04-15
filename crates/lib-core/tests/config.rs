//! Tests unitarios para `EngineConfig`.
//!
//! Cobertura:
//! - Config por defecto es válida
//! - Validación rechaza parámetros inválidos
//! - `is_large_file` clasifica correctamente
//! - `max_pipeline_ram_bytes` calcula correctamente

#[cfg(test)]
mod tests {
    use lib_core::config::EngineConfig;
    use lib_core::hash::Algorithm;

    // ── Default ───────────────────────────────────────────────────────────────

    #[test]
    fn default_config_is_valid() {
        let config = EngineConfig::default();
        assert!(
            config.validate().is_ok(),
            "La configuración por defecto debe ser válida"
        );
    }

    #[test]
    fn default_values_are_correct() {
        let c = EngineConfig::default();
        assert_eq!(c.triage_threshold_bytes, 16 * 1024 * 1024);
        assert_eq!(c.block_size_bytes,        4 * 1024 * 1024);
        assert_eq!(c.channel_capacity,        8);
        assert_eq!(c.swarm_concurrency,       128);
        assert!(!c.verify);
        assert_eq!(c.hash_algorithm,          Algorithm::Blake3);
        assert!(!c.resume);
        assert!(c.use_partial_files);
    }

    // ── Validación: casos inválidos ───────────────────────────────────────────

    #[test]
    fn zero_block_size_is_invalid() {
        let mut c = EngineConfig::default();
        c.block_size_bytes = 0;
        assert!(c.validate().is_err(), "block_size=0 debe fallar validación");
    }

    #[test]
    fn block_size_over_64mb_is_invalid() {
        let mut c = EngineConfig::default();
        c.block_size_bytes = 65 * 1024 * 1024;
        assert!(c.validate().is_err(), "block_size > 64 MB debe fallar validación");
    }

    #[test]
    fn zero_channel_capacity_is_invalid() {
        let mut c = EngineConfig::default();
        c.channel_capacity = 0;
        assert!(c.validate().is_err(), "channel_capacity=0 debe fallar validación");
    }

    #[test]
    fn zero_swarm_concurrency_is_invalid() {
        let mut c = EngineConfig::default();
        c.swarm_concurrency = 0;
        assert!(c.validate().is_err(), "swarm_concurrency=0 debe fallar validación");
    }

    #[test]
    fn swarm_over_1024_is_invalid() {
        let mut c = EngineConfig::default();
        c.swarm_concurrency = 1025;
        assert!(c.validate().is_err(), "swarm_concurrency > 1024 debe fallar validación");
    }

    #[test]
    fn pipeline_ram_over_512mb_is_invalid() {
        let mut c = EngineConfig::default();
        // 128 bloques × 8 MB = 1024 MB > 512 MB
        c.channel_capacity  = 128;
        c.block_size_bytes  = 8 * 1024 * 1024;
        assert!(c.validate().is_err(), "RAM de pipeline > 512 MB debe fallar validación");
    }

    // ── Validación: casos en el límite (boundary) ─────────────────────────────

    #[test]
    fn block_size_exactly_64mb_is_valid() {
        let mut c = EngineConfig::default();
        c.block_size_bytes = 64 * 1024 * 1024;
        assert!(c.validate().is_ok(), "block_size exactamente 64 MB debe ser válido");
    }

    #[test]
    fn swarm_exactly_1024_is_valid() {
        let mut c = EngineConfig::default();
        c.swarm_concurrency = 1024;
        assert!(c.validate().is_ok(), "swarm_concurrency=1024 debe ser válido");
    }

    // ── is_large_file ─────────────────────────────────────────────────────────

    #[test]
    fn is_large_file_at_threshold_is_large() {
        let c = EngineConfig::default();
        // Exactamente en el umbral → motor de bloques
        assert!(
            c.is_large_file(c.triage_threshold_bytes),
            "Archivo exactamente en el umbral debe clasificarse como grande"
        );
    }

    #[test]
    fn is_large_file_above_threshold_is_large() {
        let c = EngineConfig::default();
        assert!(c.is_large_file(c.triage_threshold_bytes + 1));
        assert!(c.is_large_file(1024 * 1024 * 1024)); // 1 GB
    }

    #[test]
    fn is_large_file_below_threshold_is_small() {
        let c = EngineConfig::default();
        assert!(!c.is_large_file(c.triage_threshold_bytes - 1));
        assert!(!c.is_large_file(0));
        assert!(!c.is_large_file(1));
        assert!(!c.is_large_file(1024)); // 1 KB
    }

    // ── max_pipeline_ram_bytes ────────────────────────────────────────────────

    #[test]
    fn max_pipeline_ram_is_product_of_capacity_and_block() {
        let c = EngineConfig {
            channel_capacity:  8,
            block_size_bytes:  4 * 1024 * 1024,
            ..EngineConfig::default()
        };
        assert_eq!(c.max_pipeline_ram_bytes(), 8 * 4 * 1024 * 1024); // 32 MB
    }
}