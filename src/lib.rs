//! Motor de RecupeGhost como biblioteca, compartido por los dos binarios:
//! - `recupe_ghost` (CLI, `src/main.rs`)
//! - `recupe_ghost_gui` (interfaz gráfica egui, `src/bin/recupe_ghost_gui.rs`)
//!
//! Toda la lógica de recuperación (escaneo, clonado, firmas, extracción, detección de discos)
//! vive acá y no depende de ninguna interfaz concreta. La CLI y la GUI son solo dos frentes
//! distintos sobre el mismo motor.

pub mod banner;
pub mod clone;
pub mod drives;
#[cfg(feature = "gui")]
pub mod gui;
pub mod recovery;
pub mod scanner;
pub mod signatures;
pub mod ui;
pub mod updater;
pub mod util;
