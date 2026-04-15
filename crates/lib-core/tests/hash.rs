//! Tests unitarios para el módulo de hashing.
//!
//! Cobertura:
//! - Todos los algoritmos producen output determinista
//! - Hash incremental == hash one-shot (mismos datos)
//! - Distintos datos → distintos hashes
//! - `Algorithm::from_str` parsea correctamente
//! - `HasherDispatch` produce el mismo resultado que `dyn ChecksumAlgorithm`

#[cfg(test)]
mod tests {
    use lib_core::hash::{new_hasher, Algorithm, HasherDispatch};
    use std::str::FromStr;

    const DATA_A: &[u8] = b"FileCopier-Rust hash test data block A";
    const DATA_B: &[u8] = b"FileCopier-Rust hash test data block B";
    const EMPTY:  &[u8] = b"";

    // ── Determinismo ──────────────────────────────────────────────────────────

    #[test]
    fn blake3_is_deterministic() {
        let h1 = hash_once(Algorithm::Blake3, DATA_A);
        let h2 = hash_once(Algorithm::Blake3, DATA_A);
        assert_eq!(h1, h2, "blake3: mismo input debe producir mismo hash");
    }

    #[test]
    fn xxhash_is_deterministic() {
        let h1 = hash_once(Algorithm::XxHash, DATA_A);
        let h2 = hash_once(Algorithm::XxHash, DATA_A);
        assert_eq!(h1, h2, "xxhash: mismo input debe producir mismo hash");
    }

    #[test]
    fn sha2_is_deterministic() {
        let h1 = hash_once(Algorithm::Sha2, DATA_A);
        let h2 = hash_once(Algorithm::Sha2, DATA_A);
        assert_eq!(h1, h2, "sha2: mismo input debe producir mismo hash");
    }

    // ── Distinción ────────────────────────────────────────────────────────────

    #[test]
    fn different_data_different_hashes() {
        for algo in [Algorithm::Blake3, Algorithm::XxHash, Algorithm::Sha2] {
            let h_a = hash_once(algo, DATA_A);
            let h_b = hash_once(algo, DATA_B);
            assert_ne!(h_a, h_b, "{algo}: datos distintos deben producir hashes distintos");
        }
    }

    #[test]
    fn empty_data_has_defined_hash() {
        // El hash de datos vacíos debe ser válido (no panic, no empty string)
        for algo in [Algorithm::Blake3, Algorithm::XxHash, Algorithm::Sha2] {
            let h = hash_once(algo, EMPTY);
            assert!(!h.is_empty(), "{algo}: hash de datos vacíos no debe ser string vacío");
        }
    }

    // ── Incremental vs One-shot ───────────────────────────────────────────────

    #[test]
    fn incremental_equals_oneshot_blake3() {
        // One-shot: todos los datos de una vez
        let oneshot = hash_once(Algorithm::Blake3, &[DATA_A, DATA_B].concat());

        // Incremental: datos en dos partes
        let mut h = new_hasher(Algorithm::Blake3);
        h.update(DATA_A);
        h.update(DATA_B);
        let incremental = h.finalize();

        assert_eq!(
            oneshot, incremental,
            "blake3: hash incremental debe coincidir con one-shot"
        );
    }

    #[test]
    fn incremental_equals_oneshot_xxhash() {
        let oneshot = hash_once(Algorithm::XxHash, &[DATA_A, DATA_B].concat());

        let mut h = new_hasher(Algorithm::XxHash);
        h.update(DATA_A);
        h.update(DATA_B);
        let incremental = h.finalize();

        assert_eq!(oneshot, incremental, "xxhash: hash incremental debe coincidir con one-shot");
    }

    #[test]
    fn incremental_equals_oneshot_sha2() {
        let oneshot = hash_once(Algorithm::Sha2, &[DATA_A, DATA_B].concat());

        let mut h = new_hasher(Algorithm::Sha2);
        h.update(DATA_A);
        h.update(DATA_B);
        let incremental = h.finalize();

        assert_eq!(oneshot, incremental, "sha2: hash incremental debe coincidir con one-shot");
    }

    // ── HasherDispatch ────────────────────────────────────────────────────────

    #[test]
    fn dispatch_matches_dyn_trait() {
        for algo in [Algorithm::Blake3, Algorithm::XxHash, Algorithm::Sha2] {
            // Via dyn trait
            let mut dyn_h = new_hasher(algo);
            dyn_h.update(DATA_A);
            let dyn_result = dyn_h.finalize();

            // Via HasherDispatch (dispatch estático)
            let mut static_h = HasherDispatch::new(algo);
            static_h.update(DATA_A);
            let static_result = static_h.finalize();

            assert_eq!(
                dyn_result, static_result,
                "{algo}: HasherDispatch debe producir el mismo resultado que dyn ChecksumAlgorithm"
            );
        }
    }

    // ── Formato de output ─────────────────────────────────────────────────────

    #[test]
    fn blake3_output_is_64_hex_chars() {
        let h = hash_once(Algorithm::Blake3, DATA_A);
        assert_eq!(h.len(), 64, "blake3 debe producir 64 chars hex (256 bits)");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "blake3 output debe ser hex válido");
    }

    #[test]
    fn xxhash_output_is_16_hex_chars() {
        let h = hash_once(Algorithm::XxHash, DATA_A);
        assert_eq!(h.len(), 16, "xxhash3-64 debe producir 16 chars hex (64 bits)");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "xxhash output debe ser hex válido");
    }

    #[test]
    fn sha2_output_is_64_hex_chars() {
        let h = hash_once(Algorithm::Sha2, DATA_A);
        assert_eq!(h.len(), 64, "sha2-256 debe producir 64 chars hex (256 bits)");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "sha2 output debe ser hex válido");
    }

    // ── Algorithm::from_str ───────────────────────────────────────────────────

    #[test]
    fn algorithm_from_str_valid() {
        assert_eq!(Algorithm::from_str("blake3").unwrap(),  Algorithm::Blake3);
        assert_eq!(Algorithm::from_str("BLAKE3").unwrap(),  Algorithm::Blake3);
        assert_eq!(Algorithm::from_str("xxhash").unwrap(),  Algorithm::XxHash);
        assert_eq!(Algorithm::from_str("xx").unwrap(),      Algorithm::XxHash);
        assert_eq!(Algorithm::from_str("sha2").unwrap(),    Algorithm::Sha2);
        assert_eq!(Algorithm::from_str("sha256").unwrap(),  Algorithm::Sha2);
        assert_eq!(Algorithm::from_str("SHA256").unwrap(),  Algorithm::Sha2);
    }

    #[test]
    fn algorithm_from_str_invalid() {
        assert!(Algorithm::from_str("md5").is_err());
        assert!(Algorithm::from_str("").is_err());
        assert!(Algorithm::from_str("crc32").is_err());
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn hash_once(algo: Algorithm, data: &[u8]) -> String {
        let mut h = new_hasher(algo);
        h.update(data);
        h.finalize()
    }
}