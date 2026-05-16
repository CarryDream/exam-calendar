use std::env;
use std::error::Error;
use std::path::PathBuf;

use exam_calendar::{GenerationOptions, generate_all};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("generate") => {
            let input_dir = args
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("data/calendars"));
            let output_dir = args
                .get(3)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("public/calendars"));

            let written = generate_all(&GenerationOptions {
                input_dir,
                output_dir,
            })?;

            for path in written {
                println!("{}", path.display());
            }
        }
        _ => {
            eprintln!("Usage: exam-calendar generate [input-dir] [output-dir]");
            std::process::exit(2);
        }
    }

    Ok(())
}
