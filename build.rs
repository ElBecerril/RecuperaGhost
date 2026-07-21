// Script de build. Embebe en el ejecutable de Windows la metadata de versión (ProductName,
// FileDescription, versión, autor, copyright) y, solo para la GUI, el manifiesto de elevación.
//
// Un .exe con esta información se ve como software legítimo; sin ella sale como un blob anónimo,
// lo que aumenta los falsos positivos de los antivirus (Windows Defender puso el binario en
// cuarentena, en parte, por esto).
//
// El `#[cfg(windows)]` de acá se evalúa sobre el HOST donde corre el build script. El release
// compila el target de Windows en un runner Windows nativo (host == Windows), así que la metadata
// se embebe ahí. En Linux/macOS (mi entorno de desarrollo y los otros targets del CI) es un no-op
// y ni siquiera referencia la dependencia `winresource`, para no romper la compilación.
//
// CLI vs GUI
// ----------
// Cargo corre UN build script por paquete, y `winresource` emite directivas de link que valen para
// todo el paquete (no hay forma de scopearlas por binario). Como el CLI y la GUI necesitan recursos
// DISTINTOS, nos apoyamos en que la GUI es el único que se compila con `--features gui`
// (`CARGO_FEATURE_GUI`), tanto en ci.yml como en release.yml, y el CLI nunca lo pasa.
//
// Ojo con esto: un `cargo build --features gui` SIN `--bin` compila los dos binarios y le metería
// al CLI el manifiesto de admin y el OriginalFilename de la GUI. Los workflows siempre acotan con
// `--bin recupe_ghost_gui`, así que lo que se publica está bien; el riesgo es solo de un build
// local mal invocado.

fn main() {
    // Recompilar si cambia el propio script o el feature con el que se lo invoca.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_GUI");

    #[cfg(windows)]
    embed_windows_resource();
}

// Pide privilegios de administrador al arrancar. Sin esto, la GUI muere en el primer botón con
// "Acceso denegado (os error 5)": leer un disco físico crudo (`\\.\PhysicalDriveN`) exige
// elevación, y el usuario no técnico no tiene forma de saber que hay que hacer clic derecho ->
// "Ejecutar como administrador". Con el manifiesto, Windows muestra su propio diálogo de UAC,
// que la gente sí sabe contestar.
//
// A propósito NO se declara `dpiAware`/`dpiAwareness`: winit (dentro de eframe) configura el DPI
// en runtime, y el manifiesto le gana. Mejor no pelear con eso.
// Sin declaración `<?xml ... ?>` a propósito: `winresource` escribe cada línea del manifiesto
// entre comillas y con un espacio adelante, y la declaración XML no admite NADA antes (ni un
// espacio). Es también como lo documenta el propio crate en el ejemplo de `set_manifest`.
#[cfg(windows)]
const GUI_MANIFEST: &str = r#"
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{e2011457-1546-43c5-a5fe-008deee3d3f0}" />
      <supportedOS Id="{35138b9a-5d96-4fbd-8e2d-a2440225f93a}" />
      <supportedOS Id="{4a2f28e3-53b9-4441-ba9c-d69d4a4a6e38}" />
      <supportedOS Id="{1f676c76-80e1-4239-95bb-83d0f6d0da78}" />
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}" />
    </application>
  </compatibility>
</assembly>
"#;

#[cfg(windows)]
fn embed_windows_resource() {
    // Único discriminante disponible entre los dos binarios del paquete (ver comentario de arriba).
    let is_gui = std::env::var_os("CARGO_FEATURE_GUI").is_some();

    let mut res = winresource::WindowsResource::new();
    res.set("ProductName", "RecupeGhost");
    res.set("CompanyName", "El_Becerril");
    res.set("LegalCopyright", "GPL-3.0-only - El_Becerril");

    if is_gui {
        res.set(
            "FileDescription",
            "RecupeGhost - recuperacion de archivos multimedia borrados",
        );
        // Tiene que coincidir con el nombre real del archivo: una discrepancia entre
        // OriginalFilename y el .exe publicado es justo una de las señales que los antivirus
        // puntúan negativo, y este es el binario con el que se está midiendo el falso positivo.
        res.set("OriginalFilename", "recupe_ghost_gui.exe");
        res.set_manifest(GUI_MANIFEST);
    } else {
        res.set(
            "FileDescription",
            "RecupeGhost - recuperacion de archivos multimedia borrados (linea de comandos)",
        );
        res.set("OriginalFilename", "recupe_ghost.exe");
        // El CLI NO lleva `requireAdministrator` a propósito. Con el manifiesto, ejecutarlo desde
        // una consola sin elevar falla de entrada con ERROR_ELEVATION_REQUIRED ("La operación
        // solicitada requiere elevación"), y eso rompería el modo batch y el flujo recomendado de
        // escanear una imagen .img, que no necesita permisos especiales.
    }

    if let Err(e) = res.compile() {
        // No es fatal: si por lo que sea no se puede embeber el recurso, el build sigue y el
        // binario simplemente queda sin la metadata (igual que antes).
        println!("cargo:warning=No se pudo embeber la metadata de Windows: {e}");
    }
}
