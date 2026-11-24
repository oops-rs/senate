use std::fs;
use std::path::Path;

use schemars::{Schema, schema_for};
use senate_observe_protocol::{ErrorResponse, EventFrame, ResetFrame, Snapshot};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = std::env::args_os()
        .nth(1)
        .ok_or("expected output directory")?;
    let directory = Path::new(&directory);
    fs::create_dir_all(directory)?;
    for (file, schema) in [
        ("snapshot.schema.json", schema_for!(Snapshot)),
        ("event.schema.json", schema_for!(EventFrame)),
        ("reset.schema.json", schema_for!(ResetFrame)),
        ("error.schema.json", schema_for!(ErrorResponse)),
    ] {
        fs::write(directory.join(file), pretty(&schema)?)?;
    }
    Ok(())
}

fn pretty(schema: &Schema) -> Result<String, serde_json::Error> {
    Ok(format!("{}\n", serde_json::to_string_pretty(schema)?))
}
