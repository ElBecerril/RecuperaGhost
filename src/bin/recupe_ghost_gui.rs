//! Binario de la interfaz gráfica de RecupeGhost.
//!
//! Toda la lógica vive en la biblioteca `recupe_ghost` (motor + módulo `gui`). Este binario solo
//! abre la ventana. Se compila con `cargo build --features gui`.

// Sin consola en Windows. Al abrir el programa aparecía detrás de la ventana una consola negra
// vacía: al público de esta herramienta eso le parece que algo salió mal, y a un antivirus le
// parece una aplicación de línea de comandos disfrazada de programa con ventana.
//
// Es seguro recién ahora: con la consola oculta `stdout` no existe, y cualquier `println!` en el
// camino de la GUI paniquearía. Por eso el motor tuvo que pasar antes a variantes "quiet"
// (`scan_source_quiet`, `recover_files_quiet`), que no imprimen nada.
//
// Se deja la consola en los builds de DEBUG a propósito: es donde se depura, y ahí los mensajes
// tienen que verse. Solo afecta a este binario; el CLI conserva su consola, que es donde vive.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result<()> {
    instalar_aviso_de_panico();
    recupe_ghost::gui::run()
}

/// Muestra los panics en un cartel del sistema en vez de dejarlos desaparecer.
///
/// Sin consola, el mensaje de un panic no va a ningún lado: para la persona el programa
/// simplemente se esfuma, sin nada que contar ni que reportar. El cartel al menos deja claro que
/// falló el programa y no sus archivos, y da un texto que se puede mandar por el canal.
fn instalar_aviso_de_panico() {
    let anterior = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        anterior(info);
        let detalle = info.to_string();
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("RecupeGhost tuvo un problema")
            .set_description(format!(
                "El programa se cerró por un error interno.\n\nTus archivos NO se tocaron: \
                 RecupeGhost solo lee el disco de origen, nunca escribe en él.\n\nSi podés, \
                 mandanos este detalle:\n\n{detalle}"
            ))
            .show();
    }));
}
