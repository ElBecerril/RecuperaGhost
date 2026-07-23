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

fn main() {
    instalar_aviso_de_panico();
    instalar_manejador_de_crash_nativo();

    // Si `run()` devuelve `Err` NO hay que dejarlo morir en silencio. Un `Err` de arranque no es
    // un panic, así que el hook de arriba NO lo atrapa; y como el binario no tiene consola
    // (`windows_subsystem = "windows"`), el mensaje que el runtime imprimiría por stderr no va a
    // ningún lado. Resultado sin esto: la persona hace doble clic, y "no pasa nada".
    //
    // El caso real que dispara esto es que no se pueda iniciar la parte gráfica: en una Windows
    // sin drivers de video que soporten aceleración (una VM, escritorio remoto, una PC vieja, o
    // drivers rotos) egui no consigue el contexto que necesita. Por eso el mensaje apunta a eso y
    // ofrece la salida concreta — la versión de línea de comandos, que no dibuja nada.
    if let Err(e) = recupe_ghost::gui::run() {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("RecupeGhost no pudo abrir la ventana")
            .set_description(format!(
                "No se pudo iniciar la parte gráfica del programa.\n\nCasi siempre es porque este \
                 equipo no tiene drivers de tarjeta de video actualizados (pasa en máquinas \
                 virtuales, escritorio remoto o computadoras viejas). Probá actualizar Windows y \
                 los drivers de video, y volvé a intentar.\n\nMientras tanto podés usar la versión \
                 de línea de comandos (recupe_ghost.exe), que hace lo mismo sin necesitar video.\n\n\
                 Detalle técnico (por si pedís ayuda):\n{e}"
            ))
            .show();
        std::process::exit(1);
    }
}

/// Muestra un cartel accionable ante un **crash nativo** (una excepción del sistema como un access
/// violation), que NO es un panic de Rust y por lo tanto el hook de arriba no atrapa.
///
/// El caso real que disparó esto: en una PC con gráficos Intel integrados y un driver viejo, el
/// driver de OpenGL (`ig9icd64.dll`) reventó con `0xc0000005` a mitad de un escaneo. Sin consola
/// (`windows_subsystem = "windows"`) el proceso se esfumaba sin decir nada. Con este filtro, antes
/// de morir, se explica que fue el driver de video del equipo (no los archivos del usuario) y se
/// ofrece la salida concreta: actualizar el driver o usar la versión de línea de comandos.
///
/// Solo en release de Windows: en debug hay consola (y conviene que el crash llegue al depurador),
/// y en otros sistemas no aplica.
#[cfg(all(windows, not(debug_assertions)))]
fn instalar_manejador_de_crash_nativo() {
    use std::sync::atomic::{AtomicBool, Ordering};

    // FFI mínima a `SetUnhandledExceptionFilter` (kernel32, linkeado por defecto en Windows). Se
    // evita depender de `windows-sys` como dependencia directa: solo se necesita esta función.
    #[allow(non_camel_case_types)]
    type LptopLevelExceptionFilter =
        Option<unsafe extern "system" fn(*mut core::ffi::c_void) -> i32>;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetUnhandledExceptionFilter(
            filter: LptopLevelExceptionFilter,
        ) -> LptopLevelExceptionFilter;
    }

    // EXCEPTION_EXECUTE_HANDLER: tras mostrar el cartel, terminar el proceso (sin encadenar además
    // el cartel genérico de "la aplicación dejó de funcionar" de Windows Error Reporting).
    const EXCEPTION_EXECUTE_HANDLER: i32 = 1;

    unsafe extern "system" fn filtro(_info: *mut core::ffi::c_void) -> i32 {
        // Un access violation suele dejar el heap intacto (fue un puntero malo en el driver, no
        // corrupción de memoria), así que MessageBox —que es lo que usa rfd por debajo— funciona.
        // Una sola vez, por si el propio manejo llegara a fallar en cadena.
        static YA_AVISADO: AtomicBool = AtomicBool::new(false);
        if !YA_AVISADO.swap(true, Ordering::SeqCst) {
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("RecupeGhost: falló la parte gráfica")
                .set_description(
                    "La parte gráfica de RecupeGhost se cerró por un problema con el driver de \
                     video de este equipo. Es un fallo del driver, no de tus archivos.\n\nProbá \
                     esto:\n1) Actualizá el driver de tu tarjeta o chip de video (Windows Update, \
                     o la página del fabricante).\n2) Si sigue igual, usá la versión de línea de \
                     comandos (recupe_ghost.exe): hace lo mismo sin usar la parte gráfica.\n\nLo \
                     que ya se haya guardado en la carpeta de destino sigue ahí. El disco que \
                     estabas revisando no se modificó, salvo que hayas elegido guardar en ese \
                     mismo disco.",
                )
                .show();
        }
        EXCEPTION_EXECUTE_HANDLER
    }

    unsafe {
        SetUnhandledExceptionFilter(Some(filtro));
    }
}

/// No-op fuera de release-Windows (ver la versión real más arriba).
#[cfg(not(all(windows, not(debug_assertions))))]
fn instalar_manejador_de_crash_nativo() {}

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
