// Script de build. Su única tarea es embeber metadata de versión en el ejecutable de Windows
// (ProductName, FileDescription, versión, autor, copyright). Un .exe con esta información se ve
// como software legítimo; sin ella sale como un blob anónimo, lo que aumenta los falsos positivos
// de los antivirus (Windows Defender puso el binario en cuarentena, en parte, por esto).
//
// El `#[cfg(windows)]` de acá se evalúa sobre el HOST donde corre el build script. El release
// compila el target de Windows en un runner Windows nativo (host == Windows), así que la metadata
// se embebe ahí. En Linux/macOS (mi entorno de desarrollo y los otros targets del CI) es un no-op
// y ni siquiera referencia la dependencia `winresource`, para no romper la compilación.

fn main() {
    #[cfg(windows)]
    embed_windows_resource();
}

#[cfg(windows)]
fn embed_windows_resource() {
    let mut res = winresource::WindowsResource::new();
    res.set("ProductName", "RecupeGhost");
    res.set(
        "FileDescription",
        "RecupeGhost - recuperacion de archivos multimedia borrados",
    );
    res.set("CompanyName", "El_Becerril");
    res.set("LegalCopyright", "GPL-3.0-only - El_Becerril");
    res.set("OriginalFilename", "recupe_ghost.exe");
    // ProductVersion / FileVersion se toman automaticamente de CARGO_PKG_VERSION.
    if let Err(e) = res.compile() {
        // No es fatal: si por lo que sea no se puede embeber el recurso, el build sigue y el
        // binario simplemente queda sin la metadata (igual que antes).
        println!("cargo:warning=No se pudo embeber la metadata de Windows: {e}");
    }
}
