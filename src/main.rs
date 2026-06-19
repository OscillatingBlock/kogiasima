use anyhow::Context;
use std::path::Path;

pub mod container;
use container::Container;

fn main() -> anyhow::Result<()> {
    let bundle_path = Path::new("/home/aayush/alpine_bundle");

    match Container::build_from_bundle(&bundle_path) {
        Ok(container) => {
            println!("OCI bundle parsed successfully! Starting sandbox...");
            container.run().context("Failure while running container")?;
        }
        Err(e) => {
            eprintln!("Initialization panic: {e}");
            std::process::exit(1);
        }
    }
    Ok(())
}
