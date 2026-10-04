//! On Windows, compiles `tenero.rc` (the program's icon and version information) into the .exe. Nothing happens on other systems.

fn main() {
    println!("cargo:rerun-if-changed=tenero.rc");
    println!("cargo:rerun-if-changed=../../assets/tenero.ico");
    #[cfg(windows)]
    {
        // a failure to embed must fail the build: a release with the default icon would be a silent regression
        embed_resource::compile("tenero.rc", embed_resource::NONE)
            .manifest_required()
            .expect("could not embed the icon and version information (tenero.rc)");
    }
}
