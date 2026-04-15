//! # engine
//!
//! Motor dual de copia: bloques grandes y enjambre asíncrono.
//!
//! ## Módulos
//!
//! - `orchestrator` → Triage, coordinación, checkpoint management.
//! - `block`        → Motor de bloques (archivos pesados, crossbeam pipeline).
//! - `swarm`        → Motor de enjambre (archivos pequeños, tokio).
//!
//! ## Punto de entrada
//!
//! `Orchestrator` es el único tipo público que los consumidores (CLI/GUI)
//! necesitan instanciar. Internamente despacha al motor correcto.

pub mod block;
pub mod orchestrator;
pub mod swarm;

pub use orchestrator::Orchestrator;