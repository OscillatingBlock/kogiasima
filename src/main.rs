use anyhow::Context;
use std::path::Path;

use tracing::{error, info};
use tracing_subscriber;

pub mod container;
use container::Container;
use container::cli;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = cli::build_config_file();
    let bundle_path = Path::new(&cli.output);

    match Container::build_from_bundle(&bundle_path) {
        Ok(container) => {
            info!("OCI bundle parsed successfully! Starting sandbox...");
            container.run().context("Failure while running container")?;
        }
        Err(e) => {
            error!("Initialization panic: {e}");
            std::process::exit(1);
        }
    }
    Ok(())
}
