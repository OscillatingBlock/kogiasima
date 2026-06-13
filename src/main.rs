use mini_docker::Container;
use std::path::Path;

fn main() {
    let bundle_path = Path::new("/home/aayush/alpine_bundle");

    match Container::build_from_bundle(&bundle_path) {
        Ok(container) => {
            println!("OCI bundle parsed successfully! Starting sandbox...");
            container.run();
        }
        Err(e) => {
            eprintln!("Initialization panic: {e}");
            std::process::exit(1);
        }
    }
}
