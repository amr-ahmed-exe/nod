use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=assets/nod.ico");
    embed_web();
    #[cfg(windows)]
    embed_windows_resources();
}

#[cfg(windows)]
fn embed_windows_resources() {
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/nod.ico");
    resource.set("ProductName", "Nod");
    resource.set("FileDescription", "Nod");
    resource.compile().expect("Windows resources");
}

fn embed_web() {
    let root = PathBuf::from("web");
    println!("cargo:rerun-if-changed=web");
    let mut files = Vec::new();
    collect(&root, &root, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut source = String::from("pub static EMBEDDED_WEB: &[(&str, &[u8])] = &[\n");
    let directory = std::env::current_dir().expect("current directory");
    for (relative, absolute) in files {
        let absolute = directory.join(absolute);
        println!("cargo:rerun-if-changed={}", absolute.display());
        source.push_str(&format!(
            "    ({:?}, include_bytes!({:?})),\n",
            relative,
            absolute.to_string_lossy()
        ));
    }
    source.push_str("];\n");

    let output = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("embedded_web.rs");
    std::fs::write(output, source).expect("write embedded web assets");
}

fn collect(root: &Path, directory: &Path, files: &mut Vec<(String, PathBuf)>) {
    let entries = std::fs::read_dir(directory).expect("read web directory");
    for entry in entries {
        let path = entry.expect("web entry").path();
        if path.is_dir() {
            collect(root, &path, files);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("relative web path")
                .to_string_lossy()
                .replace('\\', "/");
            files.push((relative, path));
        }
    }
}
