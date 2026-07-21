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
/// simplemente se esfuma, sin nada que contar ni que reportar.
///
/// Tres cuidados, todos aprendidos de una revisión adversarial:
///
/// 1. **Solo se avisa desde el hilo principal.** Un panic en un hilo worker del escaneo NO mata la
///    aplicación: `scan_source_impl` los recolecta y conserva lo encontrado por los demás hilos.
///    Avisar desde ahí sacaba un cartel de "el programa se cerró" mientras el programa seguía
///    funcionando, y hasta varios carteles apilados si paniqueaban varios hilos.
/// 2. **No se promete lo que no se puede cumplir.** La versión anterior decía "tus archivos no se
///    tocaron: solo leemos el disco de origen". Es falso si el usuario aceptó el riesgo de
///    guardar en el mismo disco: ahí sí se estaba escribiendo, que es justo cuando necesita saber
///    lo contrario.
/// 3. **Un solo cartel.** Un panic durante el manejo de otro panic no puede encadenar diálogos.
fn instalar_aviso_de_panico() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static YA_AVISADO: AtomicBool = AtomicBool::new(false);

    let anterior = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        anterior(info);

        let es_principal = std::thread::current().name() == Some("main");
        if !es_principal || YA_AVISADO.swap(true, Ordering::SeqCst) {
            return;
        }

        let detalle = info.to_string();
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("RecupeGhost tuvo un problema")
            .set_description(format!(
                "El programa se cerró por un error interno.\n\nLo que ya se haya guardado en la \
                 carpeta de destino sigue ahí y se puede abrir. El disco que estabas revisando no \
                 se modificó, salvo que hayas elegido guardar en ese mismo disco.\n\nSi podés, \
                 mandanos este detalle:\n\n{detalle}"
            ))
            .show();
    }));
}
