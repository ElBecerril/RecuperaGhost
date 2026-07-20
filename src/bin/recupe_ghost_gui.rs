//! Binario de la interfaz gráfica de RecupeGhost.
//!
//! Toda la lógica vive en la biblioteca `recupe_ghost` (motor + módulo `gui`). Este binario solo
//! abre la ventana. Se compila con `cargo build --features gui`.

fn main() -> eframe::Result<()> {
    recupe_ghost::gui::run()
}
